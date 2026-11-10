//! Browse screen controller: catalog loading, query, thumbnails and installs.
//!
//! Threading: everything in this module except the worker closures runs on the
//! Slint event loop. Network, decode and install work runs on worker threads
//! and posts results back with `upgrade_in_event_loop`; nothing polls.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use shiv_api::{ApiError, Client, DownloadFile, ModDto, ModQuery, SortKey, apply_query};
use shiv_core::game::{self, GameError};
use shiv_core::install::{self, InstallError, ModMeta};
use shiv_core::sidecar;
use slint::{
    ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer,
    SharedString, Timer, TimerMode, VecModel, Weak,
};

use crate::thumbs;
use crate::{App, BrowseState, DownloadRow, InstallState, LoadState, ModRow};

const SORTS: [(&str, SortKey); 5] = [
    ("Newest", SortKey::Newest),
    ("Last updated", SortKey::Updated),
    ("Most downloaded", SortKey::MostDownloaded),
    ("Most liked", SortKey::MostLiked),
    ("Name", SortKey::Name),
];
const ALL_CATEGORIES: &str = "All categories";
/// Decoded thumbnails kept in memory (about 190 at 240x135 RGBA).
const THUMB_BUDGET: usize = 24 * 1024 * 1024;
const THUMB_WORKERS: usize = 4;
/// Pending thumbnail requests; older ones (scrolled past) are dropped first.
const THUMB_QUEUE: usize = 48;
/// Blurred NSFW thumbnails kept (about 32 KB each).
const OBSCURED_KEEP: usize = 128;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
const GAMEBANANA_SS: &str = "https://images.gamebanana.com/img/ss/mods/";

/// Serialises installs: two installs must not allocate pak numbers at once.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// The controller, reachable from closures posted by worker threads.
    static BROWSE: RefCell<Option<Rc<Browse>>> = const { RefCell::new(None) };
}

fn with_browse(f: impl FnOnce(&Rc<Browse>)) {
    if let Some(b) = BROWSE.with(|cell| cell.borrow().clone()) {
        f(&b);
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers (pure, unit-tested)

/// `1234567` -> `1,234,567`.
fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// A download's size for the file picker; the API reports 0 when unknown.
fn fmt_size(bytes: Option<u64>) -> String {
    match bytes {
        None | Some(0) => "Size unknown".into(),
        Some(b) if b < 1_000_000 => format!("{} KB", b.div_ceil(1000)),
        Some(b) => fmt_mb(b),
    }
}

/// Rows for the download picker; a file without a name shows its URL's last segment.
fn download_rows(files: &[DownloadFile]) -> Vec<DownloadRow> {
    files
        .iter()
        .map(|f| {
            let name = if f.name.trim().is_empty() {
                f.url.rsplit('/').next().unwrap_or(&f.url)
            } else {
                f.name.as_str()
            };
            DownloadRow {
                name: name.into(),
                size: fmt_size(f.size).into(),
            }
        })
        .collect()
}

/// The `YYYY-MM-DD` part of an ISO-8601 timestamp (the UTC date, as the API
/// sends it); `None` for anything that does not start with a date.
pub(crate) fn short_date(iso: &str) -> Option<&str> {
    let date = iso.get(..10)?;
    let b = date.as_bytes();
    let ok = b.iter().enumerate().all(|(i, c)| match i {
        4 | 7 => *c == b'-',
        _ => c.is_ascii_digit(),
    });
    ok.then_some(date)
}

/// Thumbnail URL for a mod. GameBanana screenshots are rewritten to their
/// 530 px variant, which is a fraction of the full image's size.
fn thumb_url(m: &ModDto) -> Option<String> {
    let raw = m
        .thumbnail_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| m.images.first().map(String::as_str))
        .filter(|s| !s.is_empty())?;
    if let Some(file) = raw.strip_prefix(GAMEBANANA_SS)
        && !file.is_empty()
        && !file.contains('/')
        && !file.contains("-90_")
    {
        return Some(format!("{GAMEBANANA_SS}530-90_{file}"));
    }
    Some(raw.to_owned())
}

pub(crate) fn catalog_error_text(e: &ApiError) -> String {
    match e {
        ApiError::Http { status, .. } => {
            format!("The mod service answered with HTTP {status}. Try again in a moment.")
        }
        ApiError::Network(_) | ApiError::Io(_) => {
            "The mod service could not be reached. Check your connection and try again.".into()
        }
        ApiError::Parse(_) => "The mod service sent a response Shiv could not read.".into(),
        other => other.to_string(),
    }
}

pub(crate) fn install_error_text(e: &InstallError) -> String {
    match e {
        InstallError::GameRunning => "Close Deadlock before installing mods.".into(),
        InstallError::AddonsFull => {
            "The addons folder is full (99 paks). Remove a mod, then try again.".into()
        }
        InstallError::NoVpk => "The download contains no .vpk file to install.".into(),
        InstallError::UnsupportedArchive => "The download is not a zip, rar or 7z archive.".into(),
        InstallError::TooLarge => "The archive is too large to extract safely.".into(),
        other => other.to_string(),
    }
}

pub(crate) fn game_error_text(e: &GameError) -> String {
    match e {
        GameError::NotFound => "Deadlock was not found in any Steam library.".into(),
        other => other.to_string(),
    }
}

/// Distinct categories, most common first (ties alphabetical).
fn categories(mods: &[ModDto]) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for m in mods {
        if let Some(c) = m.category.as_deref().filter(|c| !c.is_empty()) {
            *counts.entry(c).or_default() += 1;
        }
    }
    let mut list: Vec<(&str, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    list.into_iter().map(|(c, _)| c.to_owned()).collect()
}

// ---------------------------------------------------------------------------
// Thumbnail loading

#[derive(Default)]
struct ThumbQueue {
    /// Newest request at the back; workers take from the back (LIFO) so the
    /// rows currently on screen load first.
    pending: VecDeque<String>,
    /// Pending or in flight.
    known: HashSet<String>,
}

#[derive(Clone)]
struct ThumbLoader {
    shared: Arc<(Mutex<ThumbQueue>, Condvar)>,
}

impl ThumbLoader {
    fn start(cache: Arc<thumbs::Cache>, weak: Weak<App>) -> Self {
        let loader = ThumbLoader {
            shared: Arc::new((Mutex::new(ThumbQueue::default()), Condvar::new())),
        };
        for n in 0..THUMB_WORKERS {
            let shared = loader.shared.clone();
            let cache = cache.clone();
            let weak = weak.clone();
            crate::spawn_worker(
                &format!("shiv-thumb-{n}"),
                move || thumb_worker(&shared, &cache, &weak),
                // Fetch and decode panics are caught per image in the cache;
                // anything else only costs this one worker.
                |result| {
                    if let Err(text) = result {
                        eprintln!("thumbnail worker stopped: {text}");
                    }
                },
            );
        }
        loader
    }

    fn request(&self, url: &str) {
        let (lock, cvar) = &*self.shared;
        let mut q = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if q.known.contains(url) {
            return;
        }
        q.known.insert(url.to_owned());
        q.pending.push_back(url.to_owned());
        if q.pending.len() > THUMB_QUEUE
            && let Some(old) = q.pending.pop_front()
        {
            q.known.remove(&old);
        }
        cvar.notify_one();
    }
}

fn thumb_worker(shared: &(Mutex<ThumbQueue>, Condvar), cache: &thumbs::Cache, weak: &Weak<App>) {
    let (lock, cvar) = shared;
    loop {
        let url = {
            let mut q = lock.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(url) = q.pending.pop_back() {
                    break url;
                }
                q = cvar.wait(q).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let ok = cache.get_or_fetch(&url).is_some();
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .known
            .remove(&url);
        // Failures are remembered by the cache; the row keeps its placeholder.
        if ok {
            let posted = weak.upgrade_in_event_loop(move |_| {
                with_browse(|b| b.list.thumb_ready(&url));
            });
            if posted.is_err() {
                return; // event loop is gone
            }
        }
    }
}

pub(crate) fn to_slint(img: &thumbs::Image) -> slint::Image {
    let buf =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(img.rgba(), img.width(), img.height());
    slint::Image::from_rgba8(buf)
}

// ---------------------------------------------------------------------------
// The list model

/// One row of the current result set (no description: it is only needed for search).
#[derive(Clone)]
struct Entry {
    id: String,
    remote_id: String,
    name: String,
    byline: SharedString,
    stats: SharedString,
    thumb_url: Option<String>,
    version: Option<String>,
    downloadable: bool,
    nsfw: bool,
}

impl Entry {
    fn from_dto(m: ModDto) -> Self {
        let byline = match (m.author.as_deref(), m.category.as_deref()) {
            (Some(a), Some(c)) => format!("{a} \u{b7} {c}"),
            (Some(a), None) => a.to_owned(),
            (None, Some(c)) => c.to_owned(),
            (None, None) => String::new(),
        };
        let mut stats = format!(
            "{} downloads \u{b7} {} likes",
            fmt_count(m.download_count),
            fmt_count(m.likes)
        );
        if let Some(date) = m.last_updated().and_then(short_date) {
            stats.push_str(" \u{b7} Updated ");
            stats.push_str(date);
        }
        Entry {
            thumb_url: thumb_url(&m),
            version: m.files_updated_at.or(m.remote_updated_at),
            downloadable: m.downloadable,
            nsfw: m.is_nsfw,
            byline: byline.into(),
            stats: stats.into(),
            id: m.id,
            remote_id: m.remote_id,
            name: m.name,
        }
    }
}

enum Phase {
    Downloading { progress: f32, text: String },
    Installing,
}

struct Job {
    generation: u64,
    /// The file being downloaded (read by the UI test).
    #[cfg(test)]
    url: String,
    cancel: Arc<AtomicBool>,
    phase: Phase,
}

struct ModList {
    entries: RefCell<Vec<Entry>>,
    jobs: RefCell<HashMap<String, Job>>,
    /// Mod id -> generation of the download-list request running for it.
    /// Cancel removes the entry, so its late answer is ignored.
    resolving: RefCell<HashMap<String, u64>>,
    /// Mod id -> generation of the worker still running for it. Survives
    /// Cancel and is cleared only by that worker's `Done`, so Install is
    /// refused until the previous worker has really stopped.
    in_flight: RefCell<HashMap<String, u64>>,
    installed: RefCell<HashSet<String>>,
    /// Installs finished since the last re-read of `installed` started, so a
    /// read that began before them does not drop their badge.
    installed_since: RefCell<HashSet<String>>,
    cache: Arc<thumbs::Cache>,
    loader: ThumbLoader,
    /// NSFW thumbnails are shown unblurred (off at every start).
    show_nsfw: Cell<bool>,
    /// Blurred copies of NSFW thumbnails, by URL.
    obscured: RefCell<HashMap<String, slint::Image>>,
    notify: ModelNotify,
}

impl ModList {
    fn set_entries(&self, entries: Vec<Entry>) {
        *self.entries.borrow_mut() = entries;
        self.notify.reset();
    }

    fn find(&self, id: &str) -> Option<(usize, Entry)> {
        self.entries
            .borrow()
            .iter()
            .enumerate()
            .find(|(_, e)| e.id == id)
            .map(|(i, e)| (i, e.clone()))
    }

    fn changed(&self, id: &str) {
        let idx = self.entries.borrow().iter().position(|e| e.id == id);
        if let Some(i) = idx {
            self.notify.row_changed(i);
        }
    }

    /// The thumbnail as shown: NSFW ones blurred unless the viewer opted in.
    fn display_thumb(&self, url: &str, img: &thumbs::Image, nsfw: bool) -> Option<slint::Image> {
        if !nsfw || self.show_nsfw.get() {
            return Some(to_slint(img));
        }
        if let Some(done) = self.obscured.borrow().get(url) {
            return Some(done.clone());
        }
        // No blurred copy means no thumbnail, never the original.
        let blurred = to_slint(&thumbs::obscured(img)?);
        let mut kept = self.obscured.borrow_mut();
        if kept.len() >= OBSCURED_KEEP {
            kept.clear();
        }
        kept.insert(url.to_owned(), blurred.clone());
        Some(blurred)
    }

    fn set_show_nsfw(&self, show: bool) {
        self.show_nsfw.set(show);
        if show {
            self.obscured.borrow_mut().clear();
        }
        let rows: Vec<usize> = self
            .entries
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, e)| e.nsfw)
            .map(|(i, _)| i)
            .collect();
        for i in rows {
            self.notify.row_changed(i);
        }
    }

    fn thumb_ready(&self, url: &str) {
        let rows: Vec<usize> = self
            .entries
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, e)| e.thumb_url.as_deref() == Some(url))
            .map(|(i, _)| i)
            .collect();
        for i in rows {
            self.notify.row_changed(i);
        }
    }
}

impl Model for ModList {
    type Data = ModRow;

    fn row_count(&self) -> usize {
        self.entries.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<ModRow> {
        let entries = self.entries.borrow();
        let e = entries.get(row)?;
        let mut out = ModRow {
            id: e.id.as_str().into(),
            name: e.name.as_str().into(),
            byline: e.byline.clone(),
            stats: e.stats.clone(),
            nsfw: e.nsfw,
            ..ModRow::default()
        };
        if let Some(url) = &e.thumb_url {
            match self.cache.get(url) {
                Some(img) => {
                    if let Some(thumb) = self.display_thumb(url, &img, e.nsfw) {
                        out.thumb = thumb;
                        out.has_thumb = true;
                    }
                }
                None if !self.cache.has_failed(url) => self.loader.request(url),
                None => {}
            }
        }
        if self.resolving.borrow().contains_key(&e.id) {
            out.state = InstallState::Downloading;
            out.progress = -1.0;
            out.progress_text = "Checking files".into();
            return Some(out);
        }
        out.state = match self.jobs.borrow().get(&e.id) {
            Some(job) => match &job.phase {
                Phase::Downloading { progress, text } => {
                    out.progress = *progress;
                    out.progress_text = text.as_str().into();
                    InstallState::Downloading
                }
                Phase::Installing => InstallState::Installing,
            },
            None if self.in_flight.borrow().contains_key(&e.id) => InstallState::Cancelling,
            None if self.installed.borrow().contains(&e.id) => InstallState::Installed,
            None if !e.downloadable => InstallState::Unavailable,
            None => InstallState::Idle,
        };
        Some(out)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

// ---------------------------------------------------------------------------
// Install worker

struct InstallTask {
    mod_id: String,
    file: DownloadFile,
    name: String,
    version: Option<String>,
    thumbnail_url: Option<String>,
}

enum Update {
    Progress { progress: f32, text: String },
    Installing,
    Done(Result<(), Failure>),
}

enum Failure {
    Cancelled,
    Message(String),
}

fn run_install(
    client: &Client,
    task: InstallTask,
    cancel: &AtomicBool,
    mut post: impl FnMut(Update),
) -> Result<(), Failure> {
    let msg = Failure::Message;
    let paths = crate::locate_game().map_err(|e| msg(game_error_text(&e)))?;
    // Checked before downloading too, so a long download is not wasted.
    if game::is_running() {
        return Err(msg(install_error_text(&InstallError::GameRunning)));
    }
    let file = task.file;
    let dir = tempfile::Builder::new()
        .prefix("shiv-download-")
        .tempdir()
        .map_err(|e| msg(format!("Could not create a temporary folder: {e}")))?;
    let dest = dir.path().join("archive");
    let size_hint = file.size.filter(|s| *s > 0);
    let mut last_step = u64::MAX;
    let fetched = client.fetch_cancellable(&file.url, &dest, cancel, |done, total| {
        let total = total.or(size_hint).filter(|t| *t > 0);
        // Percent steps when the size is known, otherwise one per MB.
        let step = match total {
            Some(t) => done.saturating_mul(100) / t,
            None => done / 1_000_000,
        };
        if step == last_step {
            return;
        }
        last_step = step;
        let (progress, text) = match total {
            Some(_) => (step.min(100) as f32 / 100.0, format!("{}%", step.min(100))),
            None => (-1.0, fmt_mb(done)),
        };
        post(Update::Progress { progress, text });
    });
    match fetched {
        Err(ApiError::Cancelled) => return Err(Failure::Cancelled),
        Err(e) => return Err(msg(format!("Download failed: {e}"))),
        Ok(()) => {}
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Failure::Cancelled);
    }
    post(Update::Installing);
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    // Cancel may have been pressed while waiting for another install.
    if cancel.load(Ordering::Relaxed) {
        return Err(Failure::Cancelled);
    }
    let meta = ModMeta {
        mod_id: task.mod_id,
        name: task.name,
        version: task.version,
        thumbnail_url: task.thumbnail_url,
    };
    install::install(&dest, meta, &paths).map_err(|e| msg(install_error_text(&e)))?;
    Ok(())
}

/// Mod ids with a sidecar in `addons`; empty when the game is not found.
fn installed_ids() -> HashSet<String> {
    let Ok(paths) = crate::locate_game() else {
        return HashSet::new();
    };
    sidecar::group(&paths.addons)
        .into_iter()
        .filter_map(|m| m.sidecar.map(|s| s.mod_id))
        .collect()
}

/// Re-read which mods are installed, so Install comes back after an
/// uninstall elsewhere: called when Browse is shown and after Installed's
/// toggles and uninstalls.
pub(crate) fn refresh_installed() {
    with_browse(|b| b.refresh_installed());
}

// ---------------------------------------------------------------------------
// Controller

pub struct Browse {
    weak: Weak<App>,
    client: Arc<Client>,
    list: Rc<ModList>,
    catalog: RefCell<Rc<Vec<ModDto>>>,
    categories: RefCell<Vec<String>>,
    query: RefCell<ModQuery>,
    load_generation: Cell<u64>,
    installed_generation: Cell<u64>,
    next_job: Cell<u64>,
    /// The open download picker: the mod and its files.
    picker: RefCell<Option<(String, Vec<DownloadFile>)>>,
    search_timer: Timer,
    this: RefCell<RcWeak<Browse>>,
}

/// Wire the Browse screen to `app` and start loading the catalog.
pub fn attach(app: &App, api_url: &str) -> Arc<thumbs::Cache> {
    let weak = app.as_weak();
    let cache = Arc::new(thumbs::Cache::new(THUMB_BUDGET));
    let list = Rc::new(ModList {
        entries: RefCell::new(Vec::new()),
        jobs: RefCell::new(HashMap::new()),
        resolving: RefCell::new(HashMap::new()),
        in_flight: RefCell::new(HashMap::new()),
        installed: RefCell::new(HashSet::new()),
        installed_since: RefCell::new(HashSet::new()),
        loader: ThumbLoader::start(cache.clone(), weak.clone()),
        cache: cache.clone(),
        show_nsfw: Cell::new(false),
        obscured: RefCell::new(HashMap::new()),
        notify: ModelNotify::default(),
    });
    let browse = Rc::new(Browse {
        weak,
        client: Arc::new(Client::new(api_url)),
        list: list.clone(),
        catalog: RefCell::new(Rc::new(Vec::new())),
        categories: RefCell::new(Vec::new()),
        query: RefCell::new(ModQuery {
            page_size: usize::MAX,
            ..ModQuery::default()
        }),
        load_generation: Cell::new(0),
        installed_generation: Cell::new(0),
        next_job: Cell::new(0),
        picker: RefCell::new(None),
        search_timer: Timer::default(),
        this: RefCell::new(RcWeak::new()),
    });
    *browse.this.borrow_mut() = Rc::downgrade(&browse);
    BROWSE.with(|cell| *cell.borrow_mut() = Some(browse.clone()));

    let state = app.global::<BrowseState>();
    let sorts: Vec<SharedString> = SORTS.iter().map(|(label, _)| (*label).into()).collect();
    state.set_sorts(ModelRc::new(VecModel::from(sorts)));
    state.set_categories(ModelRc::new(VecModel::from(vec![SharedString::from(
        ALL_CATEGORIES,
    )])));
    state.set_mods(ModelRc::from(list));

    let handle = |f: fn(&Rc<Browse>)| move || with_browse(f);
    state.on_search_edited(|text| with_browse(|b| b.search_edited(text.as_str())));
    state.on_sort_selected(|i| with_browse(|b| b.sort_selected(i)));
    state.on_category_selected(|i| with_browse(|b| b.category_selected(i)));
    state.on_retry(handle(|b| b.load()));
    state.on_install(|id| with_browse(|b| b.install(id.as_str())));
    state.on_cancel(|id| with_browse(|b| b.cancel(id.as_str())));
    state.on_pick_download(|i| with_browse(|b| b.pick_download(i)));
    state.on_cancel_pick(handle(|b| {
        b.close_picker();
    }));
    state.on_dismiss_banner(handle(|b| b.set_banner("")));
    state.on_shown(refresh_installed);
    state.on_toggle_nsfw(|| {
        with_browse(|b| {
            let show = !b.list.show_nsfw.get();
            b.list.set_show_nsfw(show);
            b.with_state(|s| s.set_show_nsfw(show));
        });
    });

    browse.load();
    cache
}

impl Browse {
    fn with_state(&self, f: impl FnOnce(&BrowseState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<BrowseState>());
        }
    }

    fn set_banner(&self, text: &str) {
        self.with_state(|s| s.set_banner_text(text.into()));
    }

    fn load(&self) {
        // Retry also retries thumbnails that failed while offline.
        self.list.cache.clear_failed();
        let generation = self.load_generation.get() + 1;
        self.load_generation.set(generation);
        self.with_state(|s| {
            s.set_load_state(LoadState::Loading);
            s.set_summary("".into());
        });
        let client = self.client.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-catalog",
            move || {
                let result = client.list_mods().map_err(|e| catalog_error_text(&e));
                (result, installed_ids())
            },
            move |result| {
                let (result, installed) = result.unwrap_or_else(|text| (Err(text), HashSet::new()));
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_browse(|b| b.loaded(generation, result, installed));
                });
            },
        );
        if !started {
            self.fail_load("Could not start a background thread.".into());
        }
    }

    fn refresh_installed(&self) {
        if self.weak.upgrade().is_none() {
            return;
        }
        let generation = self.installed_generation.get() + 1;
        self.installed_generation.set(generation);
        self.list.installed_since.borrow_mut().clear();
        let weak = self.weak.clone();
        crate::spawn_worker("shiv-installed-ids", installed_ids, move |result| {
            // A failed read keeps the badges as they are.
            if let Ok(ids) = result {
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_browse(|b| b.installed_read(generation, ids));
                });
            }
        });
    }

    fn installed_read(&self, generation: u64, mut ids: HashSet<String>) {
        if generation != self.installed_generation.get() {
            return;
        }
        ids.extend(self.list.installed_since.borrow_mut().drain());
        let old = std::mem::replace(&mut *self.list.installed.borrow_mut(), ids);
        let changed: Vec<usize> = {
            let now = self.list.installed.borrow();
            self.list
                .entries
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, e)| old.contains(&e.id) != now.contains(&e.id))
                .map(|(i, _)| i)
                .collect()
        };
        for i in changed {
            self.list.notify.row_changed(i);
        }
    }

    fn fail_load(&self, text: String) {
        self.with_state(|s| {
            s.set_error_text(text.into());
            s.set_load_state(LoadState::Failed);
        });
    }

    fn loaded(
        &self,
        generation: u64,
        result: Result<Vec<ModDto>, String>,
        installed: HashSet<String>,
    ) {
        if generation != self.load_generation.get() {
            return;
        }
        let mods = match result {
            Ok(mods) => mods,
            Err(text) => return self.fail_load(text),
        };
        // NSFW mods are listed with a tag and a blurred thumbnail (see
        // `ModList::display_thumb`) until the viewer turns on Show NSFW.
        if mods.is_empty() {
            return self.fail_load(
                "The mod service returned an empty catalog. Try again in a moment.".into(),
            );
        }
        let cats = categories(&mods);
        let previous = self.query.borrow().category.clone();
        let index = previous
            .as_ref()
            .and_then(|p| cats.iter().position(|c| c == p))
            .map_or(0, |i| i + 1);
        if index == 0 {
            self.query.borrow_mut().category = None;
        }
        let mut labels: Vec<SharedString> = vec![ALL_CATEGORIES.into()];
        labels.extend(cats.iter().map(|c| SharedString::from(c.as_str())));
        *self.categories.borrow_mut() = cats;
        *self.catalog.borrow_mut() = Rc::new(mods);
        *self.list.installed.borrow_mut() = installed;
        self.with_state(|s| {
            s.set_categories(ModelRc::new(VecModel::from(labels)));
            s.set_category_index(index as i32);
            s.set_load_state(LoadState::Ready);
        });
        self.refresh();
    }

    /// Re-run the query over the catalog and replace the list.
    fn refresh(&self) {
        let catalog = self.catalog.borrow().clone();
        let page = apply_query(&catalog, &self.query.borrow());
        let total = catalog.len();
        let shown = page.total;
        self.list
            .set_entries(page.items.into_iter().map(Entry::from_dto).collect());
        let summary = if shown == total {
            format!("{} mods", fmt_count(total as u64))
        } else {
            format!(
                "{} of {} mods",
                fmt_count(shown as u64),
                fmt_count(total as u64)
            )
        };
        self.with_state(|s| s.set_summary(summary.into()));
    }

    fn search_edited(&self, text: &str) {
        let search = Some(text.to_owned()).filter(|t| !t.trim().is_empty());
        self.query.borrow_mut().search = search;
        let this = self.this.borrow().clone();
        self.search_timer
            .start(TimerMode::SingleShot, SEARCH_DEBOUNCE, move || {
                if let Some(b) = this.upgrade() {
                    b.refresh();
                }
            });
    }

    fn sort_selected(&self, index: i32) {
        let sort = usize::try_from(index)
            .ok()
            .and_then(|i| SORTS.get(i))
            .map_or(SortKey::default(), |(_, k)| *k);
        self.query.borrow_mut().sort = sort;
        self.refresh();
    }

    fn category_selected(&self, index: i32) {
        let category = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.categories.borrow().get(i).cloned());
        self.query.borrow_mut().category = category;
        self.refresh();
    }

    fn busy(&self, id: &str) -> bool {
        self.list.jobs.borrow().contains_key(id)
            || self.list.in_flight.borrow().contains_key(id)
            || self.list.resolving.borrow().contains_key(id)
    }

    /// Install: fetch the mod's download list first. One file installs at
    /// once; several open the picker. Nothing is marked in flight until a
    /// file is chosen.
    fn install(&self, id: &str) {
        if self.busy(id) || self.picker.borrow().is_some() {
            return;
        }
        let Some((_, entry)) = self.list.find(id) else {
            return;
        };
        let generation = self.next_job.get() + 1;
        self.next_job.set(generation);
        self.list
            .resolving
            .borrow_mut()
            .insert(id.to_owned(), generation);
        self.list.changed(id);
        let client = self.client.clone();
        let weak = self.weak.clone();
        let mod_id = id.to_owned();
        let remote_id = entry.remote_id;
        let started = crate::spawn_worker(
            "shiv-downloads",
            move || {
                client
                    .downloads(&remote_id)
                    .map_err(|e| format!("Could not get the download list: {e}"))
            },
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_browse(|b| b.resolved(&mod_id, generation, result));
                });
            },
        );
        if !started {
            self.resolved(
                id,
                generation,
                Err("Could not start a background thread.".into()),
            );
        }
    }

    fn resolved(&self, id: &str, generation: u64, result: Result<Vec<DownloadFile>, String>) {
        {
            let mut resolving = self.list.resolving.borrow_mut();
            if resolving.get(id) != Some(&generation) {
                return; // cancelled, or superseded
            }
            resolving.remove(id);
        }
        self.list.changed(id);
        let name = self
            .list
            .find(id)
            .map_or_else(|| id.to_owned(), |(_, e)| e.name);
        let mut files = match result {
            Ok(files) => files,
            Err(text) => return self.set_banner(&format!("Could not install {name}. {text}")),
        };
        match files.len() {
            0 => self.set_banner(&format!(
                "Could not install {name}. This mod has no downloadable files."
            )),
            1 => {
                let file = files.remove(0);
                self.start_job(id, file);
            }
            _ => {
                let rows = download_rows(&files);
                *self.picker.borrow_mut() = Some((id.to_owned(), files));
                self.with_state(|s| {
                    s.set_picker_title(format!("Install {name}").into());
                    s.set_picker_files(ModelRc::new(VecModel::from(rows)));
                    s.set_picker_open(true);
                });
            }
        }
    }

    fn close_picker(&self) -> Option<(String, Vec<DownloadFile>)> {
        let open = self.picker.borrow_mut().take();
        self.with_state(|s| {
            s.set_picker_open(false);
            s.set_picker_files(ModelRc::default());
        });
        open
    }

    fn pick_download(&self, index: i32) {
        let Some((id, mut files)) = self.close_picker() else {
            return;
        };
        let Some(i) = usize::try_from(index).ok().filter(|i| *i < files.len()) else {
            return;
        };
        let file = files.swap_remove(i);
        self.start_job(&id, file);
    }

    /// Download and install `file`: the in-flight marker is set here.
    fn start_job(&self, id: &str, file: DownloadFile) {
        if self.busy(id) {
            return;
        }
        let Some((_, entry)) = self.list.find(id) else {
            return;
        };
        let generation = self.next_job.get() + 1;
        self.next_job.set(generation);
        let cancel = Arc::new(AtomicBool::new(false));
        self.list.jobs.borrow_mut().insert(
            id.to_owned(),
            Job {
                generation,
                #[cfg(test)]
                url: file.url.clone(),
                cancel: cancel.clone(),
                phase: Phase::Downloading {
                    progress: -1.0,
                    text: "Starting".into(),
                },
            },
        );
        self.list
            .in_flight
            .borrow_mut()
            .insert(id.to_owned(), generation);
        self.list.changed(id);

        let task = InstallTask {
            mod_id: entry.id.clone(),
            file,
            name: entry.name,
            version: entry.version,
            thumbnail_url: entry.thumb_url,
        };
        let client = self.client.clone();
        let weak = self.weak.clone();
        let job_id = entry.id;
        let poster = move |update: Update| {
            let id = job_id.clone();
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_browse(|b| b.job_update(&id, generation, update));
            });
        };
        let post = poster.clone();
        let started = crate::spawn_worker(
            "shiv-install",
            move || run_install(&client, task, &cancel, post),
            // `Done` clears the in-flight marker, even after a panic.
            move |result| {
                poster(Update::Done(
                    result.unwrap_or_else(|text| Err(Failure::Message(text))),
                ));
            },
        );
        if !started {
            self.job_update(
                id,
                generation,
                Update::Done(Err(Failure::Message(
                    "Could not start a background thread.".into(),
                ))),
            );
        }
    }

    fn cancel(&self, id: &str) {
        // Still fetching the download list: forget it; its answer is ignored.
        if self.list.resolving.borrow_mut().remove(id).is_some() {
            self.list.changed(id);
            return;
        }
        let removed = self.list.jobs.borrow_mut().remove(id);
        if let Some(job) = removed {
            // The worker notices at its next chunk (or right after taking the
            // install lock) and deletes the partial file. Its progress updates
            // are ignored from now on; its `Done` clears `in_flight`.
            job.cancel.store(true, Ordering::Relaxed);
            self.list.changed(id);
        }
    }

    fn job_update(&self, id: &str, generation: u64, update: Update) {
        if let Update::Done(result) = &update {
            // The disk may have changed: keep the Launch sidebar count current.
            crate::launch::refresh();
            let mut in_flight = self.list.in_flight.borrow_mut();
            if in_flight.get(id) == Some(&generation) {
                in_flight.remove(id);
            }
            // A finished install is a fact about the disk even if the job was cancelled late.
            if result.is_ok() {
                self.list.installed.borrow_mut().insert(id.to_owned());
                self.list.installed_since.borrow_mut().insert(id.to_owned());
            }
        }
        let current = self
            .list
            .jobs
            .borrow()
            .get(id)
            .is_some_and(|j| j.generation == generation);
        if !current {
            self.list.changed(id);
            return;
        }
        match update {
            Update::Progress { progress, text } => {
                if let Some(job) = self.list.jobs.borrow_mut().get_mut(id) {
                    job.phase = Phase::Downloading { progress, text };
                }
            }
            Update::Installing => {
                if let Some(job) = self.list.jobs.borrow_mut().get_mut(id) {
                    job.phase = Phase::Installing;
                }
            }
            Update::Done(result) => {
                self.list.jobs.borrow_mut().remove(id);
                if let Err(Failure::Message(text)) = result {
                    let name = self
                        .list
                        .find(id)
                        .map_or_else(|| id.to_owned(), |(_, e)| e.name);
                    self.set_banner(&format!("Could not install {name}. {text}"));
                }
            }
        }
        self.list.changed(id);
    }
}

/// Hooks for the UI test in `main.rs` (the Slint platform can only be
/// initialised once per process, so that test drives the controller).
#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;

    pub fn job_generation(id: &str) -> Option<u64> {
        let mut out = None;
        with_browse(|b| out = b.list.jobs.borrow().get(id).map(|j| j.generation));
        out
    }

    pub fn in_flight(id: &str) -> Option<u64> {
        let mut out = None;
        with_browse(|b| out = b.list.in_flight.borrow().get(id).copied());
        out
    }

    pub fn resolving(id: &str) -> bool {
        let mut out = false;
        with_browse(|b| out = b.list.resolving.borrow().contains_key(id));
        out
    }

    /// What the download-list request delivers for the current request of `id`.
    pub fn resolve(id: &str, urls: &[&str]) {
        with_browse(|b| {
            let Some(generation) = b.list.resolving.borrow().get(id).copied() else {
                return;
            };
            let files = urls
                .iter()
                .map(|u| DownloadFile {
                    url: (*u).to_owned(),
                    name: "f.zip".into(),
                    size: None,
                    md5_checksum: None,
                })
                .collect();
            b.resolved(id, generation, Ok(files));
        });
    }

    /// The file URL the running job for `id` downloads.
    pub fn job_url(id: &str) -> Option<String> {
        let mut out = None;
        with_browse(|b| out = b.list.jobs.borrow().get(id).map(|j| j.url.clone()));
        out
    }

    pub fn post_progress(id: &str, generation: u64) {
        with_browse(|b| {
            b.job_update(
                id,
                generation,
                Update::Progress {
                    progress: 0.5,
                    text: "50%".into(),
                },
            );
        });
    }

    /// What a re-read of the installed mods delivers: `stale` is a read
    /// superseded by a newer one.
    pub fn installed_read(ids: &[&str], stale: bool) {
        with_browse(|b| {
            let generation = b.installed_generation.get() + 1;
            b.installed_generation.set(generation);
            let delivered = if stale { generation - 1 } else { generation };
            b.installed_read(delivered, ids.iter().map(|s| (*s).to_owned()).collect());
        });
    }

    pub fn post_done_error(id: &str, generation: u64) {
        with_browse(|b| {
            b.job_update(
                id,
                generation,
                Update::Done(Err(Failure::Message("late failure".into()))),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped_by_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(291_020), "291,020");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn gamebanana_thumbnails_use_the_small_variant() {
        let m = |images: &[&str], thumb: Option<&str>| ModDto {
            images: images.iter().map(|s| (*s).to_owned()).collect(),
            thumbnail_url: thumb.map(str::to_owned),
            ..ModDto::default()
        };
        let gb = "https://images.gamebanana.com/img/ss/mods/6a0f.jpg";
        assert_eq!(
            thumb_url(&m(&[gb], None)).as_deref(),
            Some("https://images.gamebanana.com/img/ss/mods/530-90_6a0f.jpg")
        );
        let small = "https://images.gamebanana.com/img/ss/mods/530-90_6a0f.jpg";
        assert_eq!(thumb_url(&m(&[small], None)).as_deref(), Some(small));
        assert_eq!(
            thumb_url(&m(&[gb], Some("https://x.test/t.png"))).as_deref(),
            Some("https://x.test/t.png")
        );
        assert_eq!(thumb_url(&m(&[], Some(""))), None);
        assert_eq!(thumb_url(&m(&[], None)), None);
    }

    #[test]
    fn rows_show_the_last_updated_date() {
        assert_eq!(short_date("2026-11-08T12:30:00.000Z"), Some("2026-11-08"));
        assert_eq!(short_date("2026-11-08"), Some("2026-11-08"));
        assert_eq!(short_date("2026-1-08T00"), None);
        assert_eq!(short_date("soon"), None);
        assert_eq!(short_date(""), None);
        let m = |remote: Option<&str>, files: Option<&str>| ModDto {
            download_count: 1200,
            likes: 3,
            remote_updated_at: remote.map(str::to_owned),
            files_updated_at: files.map(str::to_owned),
            ..ModDto::default()
        };
        let stats = |d| Entry::from_dto(d).stats.to_string();
        assert_eq!(
            stats(m(
                Some("2026-11-08T01:00:00Z"),
                Some("2025-01-01T00:00:00Z")
            )),
            "1,200 downloads \u{b7} 3 likes \u{b7} Updated 2026-11-08"
        );
        assert_eq!(
            stats(m(None, Some("2025-01-01T00:00:00Z"))),
            "1,200 downloads \u{b7} 3 likes \u{b7} Updated 2025-01-01"
        );
        assert_eq!(stats(m(None, None)), "1,200 downloads \u{b7} 3 likes");
        assert_eq!(SORTS[1], ("Last updated", SortKey::Updated));
    }

    #[test]
    fn categories_most_common_first() {
        let m = |c: &str| ModDto {
            category: Some(c.to_owned()),
            ..ModDto::default()
        };
        let mods = [m("HUD"), m("Skins"), m("Skins"), m("Audio"), m("")];
        assert_eq!(categories(&mods), ["Skins", "Audio", "HUD"]);
    }

    #[test]
    fn download_picker_rows() {
        let f = |url: &str, name: &str, size: Option<u64>| DownloadFile {
            url: url.into(),
            name: name.into(),
            size,
            md5_checksum: None,
        };
        let rows = download_rows(&[
            f("https://x.test/a.zip", "Main.zip", Some(2_500_000)),
            f("https://x.test/files/alt.7z", " ", Some(0)),
            f("https://x.test/c.rar", "Tiny.rar", Some(1_500)),
            f("https://x.test/d.zip", "d.zip", None),
        ]);
        let got: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r.name.to_string(), r.size.to_string()))
            .collect();
        assert_eq!(
            got,
            [
                ("Main.zip".into(), "2.5 MB".into()),
                ("alt.7z".into(), "Size unknown".into()),
                ("Tiny.rar".into(), "2 KB".into()),
                ("d.zip".into(), "Size unknown".into()),
            ]
        );
    }

    #[test]
    fn readable_install_errors() {
        assert_eq!(
            install_error_text(&InstallError::GameRunning),
            "Close Deadlock before installing mods."
        );
        assert!(install_error_text(&InstallError::AddonsFull).contains("full"));
    }
}
