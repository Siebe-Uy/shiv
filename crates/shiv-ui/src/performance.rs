//! Performance screen controller: loads the perf catalog (API, with the last
//! good copy cached on disk by ETag), lists every setting with its value in
//! `gameinfo.gi`, stages presets and single edits as pending changes, and
//! writes them with `shiv_core::perf::apply`.
//!
//! Network and file work runs on worker threads that post back with
//! `upgrade_in_event_loop`, like the other screens. Editing is always
//! allowed; Apply is refused while Deadlock runs (checked on the worker).
//! Showing the screen re-reads `gameinfo.gi` (a game update rewrites it) and
//! keeps the pending changes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use shiv_api::Client;
use shiv_core::perf::{
    self, Catalog, CatalogCache, ConfigEntry, ConvarKind, ConvarMeta, ConvarStatus, Tier,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::browse::{catalog_error_text, game_error_text};
use crate::installed::{Locate, Running};
use crate::{App, LoadState, PerfChange, PerfControl, PerfEditor, PerfRow, PerfState, PerfTier};

/// `perf_catalog(etag)`: `Ok(None)` when the cached copy is current.
pub(crate) type Fetch =
    Arc<dyn Fn(Option<&str>) -> Result<Option<(String, Vec<u8>)>, String> + Send + Sync>;

/// Injectable so tests never touch the real install, the network or the
/// user's cache.
#[derive(Clone)]
pub(crate) struct Deps {
    pub locate: Locate,
    pub running: Running,
    pub fetch: Fetch,
    pub cache: Option<CatalogCache>,
}

impl Deps {
    fn system(api_url: &str) -> Self {
        let client = Arc::new(Client::new(api_url));
        Deps {
            locate: Arc::new(crate::locate_game),
            running: Arc::new(shiv_core::game::is_running),
            fetch: Arc::new(move |etag| {
                client
                    .perf_catalog(etag)
                    .map_err(|e| catalog_error_text(&e))
            }),
            cache: CatalogCache::default_dir().map(CatalogCache::new),
        }
    }
}

const GAME_RUNNING_TEXT: &str = "Deadlock is running. Close it, then apply again.";
const CONVARS: &str = "ConVars";

thread_local! {
    static PERF: RefCell<Option<Rc<Perf>>> = const { RefCell::new(None) };
}

fn with_perf(f: impl FnOnce(&Rc<Perf>)) {
    if let Some(p) = PERF.with(|p| p.borrow().clone()) {
        f(&p);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)

fn truthy(value: &str) -> bool {
    let v = value.trim();
    v.eq_ignore_ascii_case("true") || v.parse::<f64>().is_ok_and(|n| n != 0.0)
}

/// The opposite boolean, in the same spelling (`true`/`false` or `1`/`0`).
pub(crate) fn toggled(value: Option<&str>) -> String {
    match value.map(str::trim) {
        Some(v) if v.parse::<f64>().is_ok() => if truthy(v) { "0" } else { "1" }.into(),
        Some(v) if truthy(v) => "false".into(),
        _ => "true".into(),
    }
}

fn trim_number(text: String) -> String {
    if !text.contains('.') {
        return text;
    }
    let t = text.trim_end_matches('0').trim_end_matches('.');
    if t == "-0" || t.is_empty() {
        "0".into()
    } else {
        t.into()
    }
}

/// `v` as the convar's kind writes it: whole numbers for ints, floats to the
/// step's precision (3 places without a step).
pub(crate) fn format_number(v: f64, kind: ConvarKind, step: Option<f64>) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    if kind == ConvarKind::Int {
        return format!("{}", v.round() as i64);
    }
    let decimals = step
        .filter(|s| *s > 0.0 && s.is_finite())
        .map_or(3, |s| (-s.log10().floor()).clamp(0.0, 6.0) as usize);
    trim_number(format!("{v:.decimals$}"))
}

fn finite_range(meta: &ConvarMeta) -> Option<(f64, f64)> {
    match (meta.min, meta.max) {
        (Some(min), Some(max)) if min.is_finite() && max.is_finite() && max > min => {
            Some((min, max))
        }
        _ => None,
    }
}

/// A typed value checked against the convar's kind and range: the value to
/// stage, or why it was refused. Numbers outside the range are clamped.
pub(crate) fn checked_value(meta: Option<&ConvarMeta>, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Enter a value.".into());
    }
    if !perf::is_writable_value(text) {
        return Err("That value has characters gameinfo.gi can't hold.".into());
    }
    let Some(meta) = meta else {
        return Ok(text.into());
    };
    let number = |whole: bool| -> Result<String, String> {
        let n = text
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .filter(|n| !whole || n.fract() == 0.0)
            .ok_or_else(|| {
                if whole {
                    format!("{} takes a whole number.", meta.name)
                } else {
                    format!("{} takes a number.", meta.name)
                }
            })?;
        let n = finite_range(meta).map_or(n, |(min, max)| n.clamp(min, max));
        Ok(if whole {
            format!("{}", n as i64)
        } else if n.to_string() == text {
            text.to_string()
        } else {
            trim_number(format!("{n}"))
        })
    };
    match meta.kind {
        ConvarKind::Int => number(true),
        ConvarKind::Float => number(false),
        ConvarKind::Bool => {
            if text.eq_ignore_ascii_case("true")
                || text.eq_ignore_ascii_case("false")
                || text == "0"
                || text == "1"
            {
                Ok(text.into())
            } else {
                Err(format!("{} is on or off: true, false, 1 or 0.", meta.name))
            }
        }
        _ => Ok(text.into()),
    }
}

/// Search rank: exact name, prefix, substring, label, help. `None` when the
/// setting doesn't match.
fn rank(name_lc: &str, meta: Option<&ConvarMeta>, needle: &str) -> Option<u8> {
    if name_lc == needle {
        return Some(0);
    }
    if name_lc.starts_with(needle) {
        return Some(1);
    }
    if name_lc.contains(needle) {
        return Some(2);
    }
    let meta = meta?;
    if meta
        .label
        .as_deref()
        .is_some_and(|l| l.to_lowercase().contains(needle))
    {
        return Some(3);
    }
    meta.help_text()
        .is_some_and(|h| h.to_lowercase().contains(needle))
        .then_some(4)
}

/// `text` as a sentence: capitalised, ending in a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

fn entry_path(name: &str) -> Vec<String> {
    vec![CONVARS.into(), name.into()]
}

// ---------------------------------------------------------------------------
// Worker side

/// Where the catalog came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Api,
    /// The saved copy, because the API failed with this text.
    Saved(String),
}

type CatalogLoad = Result<(Arc<Catalog>, Origin), String>;
/// The file's ConVars (`lowercase name -> (name, value)`), or why it could
/// not be read, and whether the game runs.
type FileLoad = (Result<HashMap<String, (String, String)>, String>, bool);

fn load_catalog(deps: &Deps) -> CatalogLoad {
    let cached = deps.cache.as_ref().and_then(CatalogCache::load);
    let etag = cached.as_ref().and_then(|(e, _)| e.clone());
    let fallback = |cached: Option<(Option<String>, Catalog)>, text: String| match cached {
        Some((_, c)) => Ok((Arc::new(c), Origin::Saved(text))),
        None => Err(text),
    };
    match (deps.fetch)(etag.as_deref()) {
        Ok(None) => {
            fallback(cached, "The API sent no catalog.".into()).map(|(c, _)| (c, Origin::Api))
        }
        Ok(Some((etag, body))) => match Catalog::parse(&body) {
            Ok(catalog) => {
                if let Some(cache) = &deps.cache {
                    // A cache that can't be written only costs a download.
                    let _ = cache.save(&etag, &body);
                }
                Ok((Arc::new(catalog), Origin::Api))
            }
            Err(e) => fallback(cached, sentence(&e.to_string())),
        },
        Err(text) => fallback(cached, text),
    }
}

fn convars_of(entries: Vec<ConfigEntry>) -> HashMap<String, (String, String)> {
    entries
        .into_iter()
        .filter(|e| e.path.len() == 2 && e.path[0].eq_ignore_ascii_case(CONVARS))
        .filter_map(|e| {
            let name = e.path[1].clone();
            Some((name.to_ascii_lowercase(), (name, e.value?)))
        })
        .collect()
}

fn load_file(deps: &Deps) -> FileLoad {
    let running = (deps.running)();
    let current = (deps.locate)()
        .map_err(|e| game_error_text(&e))
        .and_then(|paths| perf::read_current(&paths).map_err(|e| sentence(&e.to_string())))
        .map(convars_of);
    (current, running)
}

/// `Err((text, game_running))`.
fn write(
    deps: &Deps,
    entries: &[ConfigEntry],
) -> Result<HashMap<String, (String, String)>, (String, bool)> {
    let paths = (deps.locate)().map_err(|e| (game_error_text(&e), false))?;
    if (deps.running)() {
        return Err((GAME_RUNNING_TEXT.into(), true));
    }
    let text = |e: perf::PerfError| (sentence(&e.to_string()), false);
    perf::apply(&paths, entries).map_err(text)?;
    perf::read_current(&paths).map(convars_of).map_err(text)
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .is_ok()
}

// ---------------------------------------------------------------------------
// Controller

struct Item {
    name: String,
    lc: String,
    meta: Option<usize>,
}

struct Perf {
    weak: Weak<App>,
    deps: Deps,
    catalog: RefCell<Option<Arc<Catalog>>>,
    origin: RefCell<Origin>,
    /// `lowercase name -> (name, value)` for the file's ConVars.
    current: RefCell<HashMap<String, (String, String)>>,
    file_error: RefCell<Option<String>>,
    items: RefCell<Vec<Item>>,
    /// Indexes into `items`, in list order.
    visible: RefCell<Vec<usize>>,
    rows: Rc<VecModel<PerfRow>>,
    changes: Rc<VecModel<PerfChange>>,
    /// Keys of `changes`, in order.
    change_keys: RefCell<Vec<String>>,
    /// Staged edits by lowercase convar name.
    pending: RefCell<BTreeMap<String, ConfigEntry>>,
    query: RefCell<String>,
    selected: RefCell<Option<String>>,
    note: RefCell<String>,
    loading: Cell<bool>,
}

/// Wire the Performance screen to `app` using the real game and API.
pub fn attach(app: &App, api_url: &str) {
    attach_with(app, Deps::system(api_url));
}

pub(crate) fn attach_with(app: &App, deps: Deps) {
    let controller = Rc::new(Perf {
        weak: app.as_weak(),
        deps,
        catalog: RefCell::new(None),
        origin: RefCell::new(Origin::Api),
        current: RefCell::new(HashMap::new()),
        file_error: RefCell::new(None),
        items: RefCell::new(Vec::new()),
        visible: RefCell::new(Vec::new()),
        rows: Rc::new(VecModel::default()),
        changes: Rc::new(VecModel::default()),
        change_keys: RefCell::new(Vec::new()),
        pending: RefCell::new(BTreeMap::new()),
        query: RefCell::new(String::new()),
        selected: RefCell::new(None),
        note: RefCell::new(String::new()),
        loading: Cell::new(false),
    });
    PERF.with(|p| *p.borrow_mut() = Some(controller.clone()));
    let state = app.global::<PerfState>();
    state.set_rows(ModelRc::from(controller.rows.clone()));
    state.set_changes(ModelRc::from(controller.changes.clone()));
    state.on_shown(|| with_perf(|p| p.shown()));
    state.on_retry(|| with_perf(|p| p.load()));
    state.on_search(|q| {
        with_perf(|p| {
            *p.query.borrow_mut() = q.to_string();
            p.refresh_list();
        });
    });
    state.on_select(|i| with_perf(|p| p.select_row(i)));
    state.on_pick_tier(|i| with_perf(|p| p.pick_tier(i)));
    state.on_set_text(|t| with_perf(|p| p.set_text(&t)));
    state.on_set_number(|v| with_perf(|p| p.set_number(f64::from(v))));
    state.on_toggle(|| with_perf(|p| p.toggle()));
    state.on_pick_option(|i| with_perf(|p| p.pick_option(i)));
    state.on_comment_out(|| with_perf(|p| p.edit_selected(|_, _| Ok(None))));
    state.on_revert(|| {
        with_perf(|p| {
            if let Some(key) = p.selected.borrow().clone() {
                p.pending.borrow_mut().remove(&key);
            }
            p.note.borrow_mut().clear();
            p.refresh_after_edit();
        });
    });
    state.on_remove_change(|i| {
        with_perf(|p| {
            let key = usize::try_from(i)
                .ok()
                .and_then(|i| p.change_keys.borrow().get(i).cloned());
            if let Some(key) = key {
                p.pending.borrow_mut().remove(&key);
                p.refresh_after_edit();
            }
        });
    });
    state.on_discard(|| {
        with_perf(|p| {
            if p.busy() {
                return;
            }
            p.pending.borrow_mut().clear();
            p.with_state(|s| s.set_active_tier(-1));
            p.refresh_after_edit();
        });
    });
    state.on_apply(|| with_perf(|p| p.apply()));
    state.on_dismiss_result(|| with_perf(|p| p.show_result("", "", false)));
    controller.publish_tiers();
    controller.publish_changes();
}

impl Perf {
    fn with_state(&self, f: impl FnOnce(&PerfState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<PerfState>());
        }
    }

    fn busy(&self) -> bool {
        self.weak
            .upgrade()
            .is_none_or(|a| a.global::<PerfState>().get_busy())
    }

    fn catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.borrow().clone()
    }

    fn shown(&self) {
        if self.catalog.borrow().is_none() {
            self.load();
        } else {
            self.reload_file();
        }
    }

    /// Load the catalog and the file.
    fn load(&self) {
        if self.loading.get() || self.busy() {
            return;
        }
        self.loading.set(true);
        self.with_state(|s| s.set_load_state(LoadState::Loading));
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-perf-load", move || {
            let catalog = load_catalog(&deps);
            let file = load_file(&deps);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_perf(|p| p.loaded(catalog, file));
            });
        });
        if !started {
            self.loaded(
                Err("Could not start a background thread.".into()),
                (Err(String::new()), false),
            );
        }
    }

    fn loaded(&self, catalog: CatalogLoad, file: FileLoad) {
        self.loading.set(false);
        match catalog {
            Ok((catalog, origin)) => {
                *self.catalog.borrow_mut() = Some(catalog);
                *self.origin.borrow_mut() = origin;
                self.with_state(|s| s.set_load_state(LoadState::Ready));
                self.publish_tiers();
                self.file_loaded(file);
            }
            Err(text) => {
                self.with_state(|s| {
                    s.set_error_text(text.into());
                    s.set_load_state(LoadState::Failed);
                });
            }
        }
    }

    /// Re-read gameinfo.gi, keeping the pending changes.
    fn reload_file(&self) {
        if self.loading.get() || self.busy() {
            return;
        }
        self.loading.set(true);
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-perf-read", move || {
            let file = load_file(&deps);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_perf(|p| {
                    p.loading.set(false);
                    p.file_loaded(file);
                });
            });
        });
        if !started {
            self.loading.set(false);
        }
    }

    fn file_loaded(&self, (current, running): FileLoad) {
        self.with_state(|s| s.set_game_running(running));
        match current {
            Ok(current) => {
                *self.current.borrow_mut() = current;
                *self.file_error.borrow_mut() = None;
            }
            Err(text) => {
                self.current.borrow_mut().clear();
                *self.file_error.borrow_mut() = Some(text);
            }
        }
        self.prune_pending();
        self.rebuild_items();
        self.refresh_list();
        self.publish_changes();
        self.publish_editor();
        self.publish_status();
    }

    fn publish_status(&self) {
        let Some(catalog) = self.catalog() else {
            return;
        };
        let source = match &*self.origin.borrow() {
            Origin::Api => format!("Catalog {}", catalog.version()),
            Origin::Saved(why) => format!(
                "Catalog {} (saved copy: {})",
                catalog.version(),
                why.trim_end_matches('.')
            ),
        };
        let file = match &*self.file_error.borrow() {
            Some(e) => format!("gameinfo.gi could not be read: {e}"),
            None => format!(
                "{} settings set in gameinfo.gi",
                self.current.borrow().len()
            ),
        };
        self.with_state(|s| s.set_status(format!("{source} · {file}").into()));
    }

    fn publish_tiers(&self) {
        let catalog = self.catalog();
        let tiers: Vec<PerfTier> = Tier::ALL
            .iter()
            .map(|&tier| {
                let preset = catalog.as_ref().and_then(|c| c.preset_info(tier));
                let count = catalog.as_ref().map_or(0, |c| c.preset(tier).len());
                PerfTier {
                    label: tier.label().into(),
                    name: preset
                        .map(|p| format!("{} · {count} settings", p.name))
                        .unwrap_or_default()
                        .into(),
                    blurb: preset.map(|p| p.blurb.clone()).unwrap_or_default().into(),
                    available: count > 0,
                }
            })
            .collect();
        self.with_state(|s| s.set_tiers(ModelRc::new(VecModel::from(tiers))));
    }

    /// Every catalog convar the game still has, plus any ConVars key the file
    /// sets that the catalog doesn't know.
    fn rebuild_items(&self) {
        let Some(catalog) = self.catalog() else {
            return;
        };
        let current = self.current.borrow();
        let mut items: Vec<Item> = catalog
            .convars()
            .iter()
            .enumerate()
            .filter_map(|(i, meta)| {
                let lc = meta.name.to_ascii_lowercase();
                let gone = matches!(meta.status, ConvarStatus::Removed | ConvarStatus::NotConvar);
                (!gone || current.contains_key(&lc)).then(|| Item {
                    name: meta.name.clone(),
                    lc,
                    meta: Some(i),
                })
            })
            .collect();
        for (lc, (name, _)) in current.iter() {
            if catalog.convar(lc).is_none() {
                items.push(Item {
                    name: name.clone(),
                    lc: lc.clone(),
                    meta: None,
                });
            }
        }
        items.sort_by(|a, b| a.lc.cmp(&b.lc));
        *self.items.borrow_mut() = items;
    }

    fn meta(&self, item: &Item) -> Option<ConvarMeta> {
        let catalog = self.catalog()?;
        item.meta.and_then(|i| catalog.convars().get(i).cloned())
    }

    /// Filter and order the list for the query: without one, settings the
    /// file sets come first.
    fn refresh_list(&self) {
        let needle = self.query.borrow().trim().to_lowercase();
        let catalog = self.catalog();
        let items = self.items.borrow();
        let current = self.current.borrow();
        let mut ranked: Vec<(u8, usize)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                if needle.is_empty() {
                    let set = current.contains_key(&item.lc)
                        || self.pending.borrow().contains_key(&item.lc);
                    return Some((u8::from(!set), i));
                }
                let meta = item
                    .meta
                    .and_then(|m| catalog.as_ref().and_then(|c| c.convars().get(m)));
                rank(&item.lc, meta, &needle).map(|r| (r, i))
            })
            .collect();
        ranked.sort_by_key(|&(r, i)| (r, i));
        let visible: Vec<usize> = ranked.into_iter().map(|(_, i)| i).collect();
        let note = if needle.is_empty() {
            format!(
                "{} settings · the ones gameinfo.gi sets come first",
                visible.len()
            )
        } else if visible.is_empty() {
            "No settings match".into()
        } else {
            format!("{} matching settings", visible.len())
        };
        let rows: Vec<PerfRow> = visible.iter().map(|&i| self.row(&items[i])).collect();
        drop((items, current));
        *self.visible.borrow_mut() = visible;
        self.rows.set_vec(rows);
        self.with_state(|s| s.set_list_note(note.into()));
    }

    fn row(&self, item: &Item) -> PerfRow {
        let catalog = self.catalog();
        let meta = item
            .meta
            .and_then(|m| catalog.as_ref().and_then(|c| c.convars().get(m)));
        let detail = meta
            .and_then(|m| m.label.as_deref().or(m.help_text()))
            .map(first_line)
            .unwrap_or_default();
        let current = self
            .current
            .borrow()
            .get(&item.lc)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let pending = self.pending.borrow().get(&item.lc).map(|e| match &e.value {
            Some(v) => v.clone(),
            None => "off".into(),
        });
        PerfRow {
            name: item.name.as_str().into(),
            detail: detail.into(),
            current: current.into(),
            pending: pending.unwrap_or_default().into(),
            selected: self.selected.borrow().as_deref() == Some(item.lc.as_str()),
        }
    }

    /// Re-draw the visible rows for these keys only.
    fn refresh_rows(&self, keys: &[&str]) {
        let items = self.items.borrow();
        for (row, &i) in self.visible.borrow().iter().enumerate() {
            if keys.contains(&items[i].lc.as_str()) {
                self.rows.set_row_data(row, self.row(&items[i]));
            }
        }
    }

    fn select_row(&self, row: i32) {
        let key = usize::try_from(row)
            .ok()
            .and_then(|r| self.visible.borrow().get(r).copied())
            .map(|i| self.items.borrow()[i].lc.clone());
        let Some(key) = key else {
            return;
        };
        let previous = self.selected.replace(Some(key.clone()));
        self.note.borrow_mut().clear();
        let mut keys = vec![key.as_str()];
        if let Some(p) = previous.as_deref() {
            keys.push(p);
        }
        self.refresh_rows(&keys);
        self.publish_editor();
    }

    fn selected_item(&self) -> Option<(String, String, Option<ConvarMeta>)> {
        let key = self.selected.borrow().clone()?;
        let items = self.items.borrow();
        let item = items.iter().find(|i| i.lc == key)?;
        Some((key, item.name.clone(), self.meta(item)))
    }

    /// The value the editor shows: pending, else the file's, else the default.
    fn shown_value(&self, key: &str, meta: Option<&ConvarMeta>) -> Option<String> {
        if let Some(e) = self.pending.borrow().get(key) {
            return e.value.clone();
        }
        self.current
            .borrow()
            .get(key)
            .map(|(_, v)| v.clone())
            .or_else(|| meta.and_then(|m| m.default.clone()))
    }

    fn publish_editor(&self) {
        let Some((key, name, meta)) = self.selected_item() else {
            self.with_state(|s| s.set_editor(PerfEditor::default()));
            return;
        };
        let catalog = self.catalog();
        let current = self.current.borrow().get(&key).map(|(_, v)| v.clone());
        let value = self.shown_value(&key, meta.as_ref());
        let pending = self.pending.borrow().get(&key).cloned();
        let denied = catalog
            .as_ref()
            .and_then(|c| c.writable(&entry_path(&name)).err());

        let mut meta_line = match &meta {
            Some(m) => format!(
                "{} · {}",
                catalog
                    .as_ref()
                    .map_or(m.category.as_str(), |c| c.category_label(&m.category)),
                kind_label(m.kind)
            ),
            None => "Not in the catalog".into(),
        };
        match meta.as_ref().map(|m| m.status) {
            Some(ConvarStatus::Blocked) => meta_line.push_str(" · blocked by the game"),
            Some(ConvarStatus::Removed) => meta_line.push_str(" · not in the current game build"),
            Some(ConvarStatus::NotConvar) => meta_line.push_str(" · a console command"),
            _ => {}
        }
        let mut help = meta
            .as_ref()
            .and_then(|m| m.help_text())
            .unwrap_or_default()
            .to_string();
        if let Some(side) = meta.as_ref().and_then(|m| m.side_effects.as_deref()) {
            if !help.is_empty() {
                help.push_str("\n\n");
            }
            help.push_str(side);
        }
        let range = meta.as_ref().and_then(finite_range);
        let mut limits = Vec::new();
        if let (Some(m), Some((min, max))) = (&meta, range) {
            limits.push(format!(
                "Range {} to {}",
                format_number(min, m.kind, m.step),
                format_number(max, m.kind, m.step)
            ));
        }
        if let Some(d) = meta.as_ref().and_then(|m| m.default.as_deref()) {
            limits.push(format!("default {d}"));
        }

        let kind = meta.as_ref().map(|m| m.kind);
        let options: Vec<&perf::EnumValue> = meta
            .as_ref()
            .map(|m| m.enum_values.iter().collect())
            .unwrap_or_default();
        let control = if denied.is_some() {
            PerfControl::Locked
        } else if kind == Some(ConvarKind::Bool) {
            PerfControl::Toggle
        } else if kind == Some(ConvarKind::Enum) && !options.is_empty() {
            PerfControl::Choice
        } else if matches!(kind, Some(ConvarKind::Int | ConvarKind::Float)) && range.is_some() {
            PerfControl::Slider
        } else {
            PerfControl::Text
        };
        let (minimum, maximum) = range.unwrap_or((0.0, 1.0));
        let step = meta
            .as_ref()
            .and_then(|m| m.step)
            .filter(|s| *s > 0.0)
            .unwrap_or(if kind == Some(ConvarKind::Int) {
                1.0
            } else {
                (maximum - minimum) / 100.0
            });
        let shown = value.clone().unwrap_or_default();
        let number = shown
            .parse::<f64>()
            .unwrap_or(minimum)
            .clamp(minimum, maximum);
        let option_index = options
            .iter()
            .position(|o| o.value == shown)
            .unwrap_or_default();
        let note = denied
            .map(|reason| format!("Shiv doesn't change this setting: {reason}"))
            .unwrap_or_else(|| self.note.borrow().clone());
        let editor = PerfEditor {
            open: true,
            name: name.into(),
            meta: meta_line.into(),
            help: help.into(),
            limits: limits.join(" · ").into(),
            current: match &current {
                Some(v) => format!("In gameinfo.gi: {v}"),
                None => "Not set in gameinfo.gi".into(),
            }
            .into(),
            control,
            value: if pending.as_ref().is_some_and(|p| p.value.is_none()) {
                SharedString::new()
            } else {
                shown.as_str().into()
            },
            checked: value.as_deref().is_some_and(truthy),
            number: number as f32,
            minimum: minimum as f32,
            maximum: maximum as f32,
            step: step as f32,
            options: ModelRc::new(VecModel::from(
                options
                    .iter()
                    .map(|o| SharedString::from(o.label.as_str()))
                    .collect::<Vec<_>>(),
            )),
            option_index: option_index as i32,
            has_pending: pending.is_some(),
            is_set: current.is_some() && !pending.as_ref().is_some_and(|p| p.value.is_none()),
            note: note.into(),
        };
        self.with_state(|s| s.set_editor(editor));
    }

    /// Stage `edit(meta, shown value)` for the selected setting. `Err` shows
    /// as the editor's note and stages nothing.
    fn edit_selected(
        &self,
        edit: impl FnOnce(Option<&ConvarMeta>, Option<String>) -> Result<Option<String>, String>,
    ) {
        if self.busy() {
            return;
        }
        let Some((key, name, meta)) = self.selected_item() else {
            return;
        };
        let denied = self
            .catalog()
            .and_then(|c| c.writable(&entry_path(&name)).err());
        if denied.is_some() {
            return;
        }
        let shown = self.shown_value(&key, meta.as_ref());
        match edit(meta.as_ref(), shown) {
            Ok(value) => {
                self.note.borrow_mut().clear();
                self.stage(&key, &name, value);
            }
            Err(text) => *self.note.borrow_mut() = text,
        }
        self.refresh_after_edit();
    }

    /// Stage a value, or drop the change when the file already has it.
    fn stage(&self, key: &str, name: &str, value: Option<String>) {
        let unchanged = {
            let current = self.current.borrow();
            match (&value, current.get(key)) {
                (Some(v), Some((_, c))) => perf::values_equal(v, c),
                (None, None) => true,
                _ => false,
            }
        };
        let mut pending = self.pending.borrow_mut();
        if unchanged {
            pending.remove(key);
        } else {
            pending.insert(
                key.to_string(),
                ConfigEntry {
                    path: entry_path(name),
                    value,
                },
            );
        }
    }

    /// Drop pending changes the file now already has.
    fn prune_pending(&self) {
        let staged: Vec<(String, ConfigEntry)> = self
            .pending
            .borrow()
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        for (key, entry) in staged {
            self.stage(&key, &entry.path[1], entry.value);
        }
    }

    fn refresh_after_edit(&self) {
        let selected = self.selected.borrow().clone();
        let keys: Vec<String> = self
            .change_keys
            .borrow()
            .iter()
            .cloned()
            .chain(self.pending.borrow().keys().cloned())
            .chain(selected)
            .collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.refresh_rows(&keys);
        self.publish_changes();
        self.publish_editor();
    }

    fn publish_changes(&self) {
        let current = self.current.borrow();
        let pending = self.pending.borrow();
        let rows: Vec<PerfChange> = pending
            .iter()
            .map(|(key, e)| PerfChange {
                name: e.path[1].as_str().into(),
                before: current
                    .get(key)
                    .map_or_else(|| "not set".to_string(), |(_, v)| v.clone())
                    .into(),
                after: e
                    .value
                    .clone()
                    .unwrap_or_else(|| "commented out".into())
                    .into(),
            })
            .collect();
        let label = match pending.len() {
            0 => "Apply".to_string(),
            1 => "Apply 1 change".to_string(),
            n => format!("Apply {n} changes"),
        };
        *self.change_keys.borrow_mut() = pending.keys().cloned().collect();
        drop((current, pending));
        self.changes.set_vec(rows);
        self.with_state(|s| s.set_apply_label(label.into()));
    }

    fn pick_tier(&self, index: i32) {
        if self.busy() {
            return;
        }
        let Some(tier) = usize::try_from(index).ok().and_then(|i| Tier::ALL.get(i)) else {
            return;
        };
        let Some(catalog) = self.catalog() else {
            return;
        };
        let entries = catalog.preset(*tier);
        if entries.is_empty() {
            return;
        }
        self.pending.borrow_mut().clear();
        for e in entries {
            self.stage(&e.path[1].to_ascii_lowercase(), &e.path[1], e.value);
        }
        self.note.borrow_mut().clear();
        let blurb = catalog
            .preset_info(*tier)
            .map(|p| format!("{} by {}: {}", p.name, p.author, p.blurb))
            .unwrap_or_default();
        self.with_state(|s| {
            s.set_active_tier(index);
            s.set_result_title(format!("{} preset staged", tier.label()).into());
            s.set_result_detail(format!("{blurb} Review the pending changes, then apply.").into());
            s.set_result_error(false);
        });
        self.refresh_list();
        self.publish_changes();
        self.publish_editor();
    }

    fn set_text(&self, text: &str) {
        self.edit_selected(|meta, _| checked_value(meta, text).map(Some));
    }

    fn set_number(&self, v: f64) {
        self.edit_selected(|meta, _| {
            let meta = meta.ok_or_else(String::new)?;
            let v = finite_range(meta).map_or(v, |(min, max)| v.clamp(min, max));
            Ok(Some(format_number(v, meta.kind, meta.step)))
        });
    }

    fn toggle(&self) {
        self.edit_selected(|_, shown| Ok(Some(toggled(shown.as_deref()))));
    }

    fn pick_option(&self, index: i32) {
        self.edit_selected(|meta, _| {
            let option = meta
                .zip(usize::try_from(index).ok())
                .and_then(|(m, i)| m.enum_values.get(i))
                .ok_or_else(String::new)?;
            Ok(Some(option.value.clone()))
        });
    }

    fn apply(&self) {
        if self.busy() || self.loading.get() {
            return;
        }
        let entries: Vec<ConfigEntry> = self.pending.borrow().values().cloned().collect();
        if entries.is_empty() {
            return;
        }
        self.with_state(|s| s.set_busy(true));
        self.show_result("", "", false);
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let count = entries.len();
        let started = spawn("shiv-perf-apply", move || {
            let result = write(&deps, &entries);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_perf(|p| p.applied(count, result));
            });
        });
        if !started {
            self.applied(
                count,
                Err(("Could not start a background thread.".into(), false)),
            );
        }
    }

    fn applied(
        &self,
        count: usize,
        result: Result<HashMap<String, (String, String)>, (String, bool)>,
    ) {
        self.with_state(|s| s.set_busy(false));
        match result {
            Ok(current) => {
                self.pending.borrow_mut().clear();
                self.with_state(|s| {
                    s.set_active_tier(-1);
                    s.set_game_running(false);
                });
                self.file_loaded((Ok(current), false));
                let what = if count == 1 {
                    "1 change".to_string()
                } else {
                    format!("{count} changes")
                };
                self.show_result(
                    "Settings applied",
                    &format!(
                        "Wrote {what} to gameinfo.gi and kept the previous file as gameinfo.gi.bak. \
                         Deadlock uses them the next time it starts. A game update replaces \
                         gameinfo.gi, so apply again after one."
                    ),
                    false,
                );
            }
            Err((text, running)) => {
                if running {
                    self.with_state(|s| s.set_game_running(true));
                }
                self.show_result("Couldn't apply the settings", &text, true);
            }
        }
    }

    fn show_result(&self, title: &str, detail: &str, error: bool) {
        self.with_state(|s| {
            s.set_result_title(title.into());
            s.set_result_detail(detail.into());
            s.set_result_error(error);
        });
    }
}

fn kind_label(kind: ConvarKind) -> &'static str {
    match kind {
        ConvarKind::Bool => "on or off",
        ConvarKind::Int => "whole number",
        ConvarKind::Float => "number",
        ConvarKind::String => "text",
        ConvarKind::Enum => "choice",
        ConvarKind::Color => "colour",
        ConvarKind::Vector => "vector",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: ConvarKind, min: Option<f64>, max: Option<f64>) -> ConvarMeta {
        ConvarMeta {
            name: "x".into(),
            kind,
            default: None,
            min,
            max,
            step: None,
            enum_values: Vec::new(),
            help: None,
            label: None,
            description: None,
            category: "other".into(),
            side_effects: None,
            status: ConvarStatus::Active,
        }
    }

    fn deps(fetch: Fetch, cache: CatalogCache) -> Deps {
        Deps {
            locate: Arc::new(|| Err(shiv_core::game::GameError::NotFound)),
            running: Arc::new(|| false),
            fetch,
            cache: Some(cache),
        }
    }

    #[test]
    fn catalog_uses_the_etag_and_falls_back_to_the_saved_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = CatalogCache::new(dir.path().to_path_buf());
        let body = crate::tests::PERF_CATALOG.as_bytes().to_vec();
        // Nothing saved and the API down: an error.
        let down: Fetch = Arc::new(|_| Err("The mod service could not be reached.".into()));
        assert!(load_catalog(&deps(down.clone(), cache.clone())).is_err());
        // A download is saved with its ETag.
        let sent = body.clone();
        let fresh: Fetch = Arc::new(move |etag| {
            assert_eq!(etag, None);
            Ok(Some(("\"e1\"".into(), sent.clone())))
        });
        let (catalog, origin) = load_catalog(&deps(fresh, cache.clone())).expect("fresh");
        assert_eq!(origin, Origin::Api);
        assert_eq!(catalog.version(), "2026.11.07-1");
        // Not modified: the saved copy, asked for by its ETag.
        let same: Fetch = Arc::new(|etag| {
            assert_eq!(etag, Some("\"e1\""));
            Ok(None)
        });
        assert_eq!(
            load_catalog(&deps(same, cache.clone())).expect("304").1,
            Origin::Api
        );
        // API down, or a catalog from a newer schema: the saved copy, said so.
        let (_, origin) = load_catalog(&deps(down, cache.clone())).expect("offline");
        assert!(matches!(origin, Origin::Saved(ref t) if t.contains("could not be reached")));
        let newer = String::from_utf8(body)
            .expect("utf8")
            .replace("\"schemaVersion\":1", "\"schemaVersion\":2");
        let newer: Fetch =
            Arc::new(move |_| Ok(Some(("\"e2\"".into(), newer.clone().into_bytes()))));
        let (catalog, origin) = load_catalog(&deps(newer, cache.clone())).expect("newer schema");
        assert!(
            matches!(origin, Origin::Saved(ref t) if t.contains("schema version 2")),
            "{origin:?}"
        );
        assert_eq!(catalog.version(), "2026.11.07-1");
        assert_eq!(cache.load().expect("kept").0.as_deref(), Some("\"e1\""));
    }

    #[test]
    fn toggling_keeps_the_spelling() {
        assert_eq!(toggled(Some("true")), "false");
        assert_eq!(toggled(Some("FALSE")), "true");
        assert_eq!(toggled(Some("1")), "0");
        assert_eq!(toggled(Some("0")), "1");
        assert_eq!(toggled(None), "true");
    }

    #[test]
    fn numbers_follow_kind_and_step() {
        assert_eq!(format_number(3.6, ConvarKind::Int, None), "4");
        assert_eq!(format_number(0.30000001, ConvarKind::Float, None), "0.3");
        assert_eq!(format_number(0.26, ConvarKind::Float, Some(0.5)), "0.3");
        assert_eq!(format_number(12.0, ConvarKind::Float, Some(1.0)), "12");
        assert_eq!(format_number(-0.0001, ConvarKind::Float, None), "0");
    }

    #[test]
    fn typed_values_are_checked_against_kind_and_range() {
        let int = meta(ConvarKind::Int, Some(0.0), Some(1000.0));
        assert_eq!(checked_value(Some(&int), " 240 ").unwrap(), "240");
        assert_eq!(checked_value(Some(&int), "5000").unwrap(), "1000");
        assert!(checked_value(Some(&int), "1.5").is_err());
        assert!(checked_value(Some(&int), "fast").is_err());
        let float = meta(ConvarKind::Float, None, None);
        assert_eq!(checked_value(Some(&float), "0.50").unwrap(), "0.5");
        assert_eq!(checked_value(Some(&float), "1e-3").unwrap(), "0.001");
        let text = meta(ConvarKind::String, None, None);
        assert_eq!(checked_value(Some(&text), "a b").unwrap(), "a b");
        assert!(checked_value(Some(&text), "a\"b").is_err());
        assert!(checked_value(None, "").is_err());
        assert!(checked_value(Some(&meta(ConvarKind::Bool, None, None)), "yes").is_err());
    }

    #[test]
    fn search_ranks_names_before_help() {
        let mut m = meta(ConvarKind::Bool, None, None);
        m.help = Some("Ambient occlusion".into());
        assert_eq!(rank("r_ssao", Some(&m), "r_ssao"), Some(0));
        assert_eq!(rank("r_ssao", Some(&m), "r_s"), Some(1));
        assert_eq!(rank("r_ssao", Some(&m), "ssao"), Some(2));
        assert_eq!(rank("r_ssao", Some(&m), "occlusion"), Some(4));
        assert_eq!(rank("r_ssao", Some(&m), "shadow"), None);
    }
}
