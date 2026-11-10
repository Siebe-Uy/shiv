//! The detail page's Files, Changelog and Comments tabs.
//!
//! They load from GameBanana the first time a tab is opened (and page by page
//! for the two long lists), never while the page merely opens. Results that
//! arrive after the page was closed or another mod opened are dropped.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use shiv_api::{ChangelogEntry, Comment, Comments, GameBananaClient, ModFile, Page};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::browse::{fmt_count, fmt_size, now_epoch, relative_secs};
use crate::{App, BrowseState, TabLoad, TextRow};

/// Longest comment or change text shown, in characters.
const MAX_BODY: usize = 3_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Files,
    Changelog,
    Comments,
}

impl Tab {
    fn from_index(i: i32) -> Option<Tab> {
        match i {
            1 => Some(Tab::Files),
            2 => Some(Tab::Changelog),
            3 => Some(Tab::Comments),
            _ => None,
        }
    }
}

enum Fetched {
    Files(Vec<ModFile>),
    Changes(Page<ChangelogEntry>),
    Comments(Comments),
}

struct Tabs {
    weak: Weak<App>,
    client: Arc<GameBananaClient>,
    /// The open mod: GameBanana id and whether it is a sound.
    target: RefCell<Option<(String, bool)>>,
    generation: Cell<u64>,
    files: Rc<VecModel<TextRow>>,
    changes: Rc<VecModel<TextRow>>,
    comments: Rc<VecModel<TextRow>>,
    changes_page: Cell<u32>,
    comments_page: Cell<u32>,
}

thread_local! {
    static TABS: RefCell<Option<Rc<Tabs>>> = const { RefCell::new(None) };
}

fn with_tabs(f: impl FnOnce(&Rc<Tabs>)) {
    if let Some(t) = TABS.with(|c| c.borrow().clone()) {
        f(&t);
    }
}

/// Wire the tabs to `app`, reading GameBanana's public API.
pub fn attach(app: &App) {
    attach_with(app, GameBananaClient::default());
}

pub(crate) fn attach_with(app: &App, client: GameBananaClient) {
    let tabs = Rc::new(Tabs {
        weak: app.as_weak(),
        client: Arc::new(client),
        target: RefCell::new(None),
        generation: Cell::new(0),
        files: Rc::new(VecModel::default()),
        changes: Rc::new(VecModel::default()),
        comments: Rc::new(VecModel::default()),
        changes_page: Cell::new(0),
        comments_page: Cell::new(0),
    });
    let state = app.global::<BrowseState>();
    state.set_files(ModelRc::from(tabs.files.clone()));
    state.set_changes(ModelRc::from(tabs.changes.clone()));
    state.set_comments(ModelRc::from(tabs.comments.clone()));
    TABS.with(|c| *c.borrow_mut() = Some(tabs));
    state.on_detail_tab_selected(|i| with_tabs(|t| t.select(i)));
    state.on_tab_more(|i| with_tabs(|t| t.fetch_next(i)));
    state.on_tab_retry(|i| with_tabs(|t| t.retry(i)));
}

/// A mod's page opened: start from the Description tab with nothing loaded.
pub(crate) fn open(remote_id: &str, audio: bool) {
    with_tabs(|t| t.reset(Some((remote_id.to_owned(), audio))));
}

/// Developer probe: switch the open page to a tab.
#[cfg(any(debug_assertions, feature = "dev-probe"))]
pub(crate) fn select_for_probe(index: i32) {
    with_tabs(|t| t.select(index));
}

/// The page closed: drop everything it loaded.
pub(crate) fn close() {
    with_tabs(|t| t.reset(None));
}

impl Tabs {
    fn with_state(&self, f: impl FnOnce(&BrowseState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<BrowseState>());
        }
    }

    fn reset(&self, target: Option<(String, bool)>) {
        self.generation.set(self.generation.get() + 1);
        *self.target.borrow_mut() = target;
        self.files.set_vec(Vec::new());
        self.changes.set_vec(Vec::new());
        self.comments.set_vec(Vec::new());
        self.changes_page.set(0);
        self.comments_page.set(0);
        self.with_state(|s| {
            s.set_detail_tab(0);
            s.set_files_load(TabLoad::Idle);
            s.set_changes_load(TabLoad::Idle);
            s.set_comments_load(TabLoad::Idle);
            s.set_changes_more(false);
            s.set_comments_more(false);
        });
    }

    fn load_state(&self, tab: Tab) -> TabLoad {
        self.weak.upgrade().map_or(TabLoad::Idle, |app| {
            let s = app.global::<BrowseState>();
            match tab {
                Tab::Files => s.get_files_load(),
                Tab::Changelog => s.get_changes_load(),
                Tab::Comments => s.get_comments_load(),
            }
        })
    }

    fn set_load(&self, tab: Tab, load: TabLoad) {
        self.with_state(|s| match tab {
            Tab::Files => s.set_files_load(load),
            Tab::Changelog => s.set_changes_load(load),
            Tab::Comments => s.set_comments_load(load),
        });
    }

    fn select(&self, index: i32) {
        self.with_state(|s| s.set_detail_tab(index));
        if let Some(tab) = Tab::from_index(index)
            && self.load_state(tab) == TabLoad::Idle
        {
            self.fetch(tab, 1);
        }
    }

    fn fetch_next(&self, index: i32) {
        let Some(tab) = Tab::from_index(index) else {
            return;
        };
        if self.load_state(tab) == TabLoad::Loading {
            return;
        }
        match tab {
            Tab::Changelog => self.fetch(tab, self.changes_page.get() + 1),
            Tab::Comments => self.fetch(tab, self.comments_page.get() + 1),
            Tab::Files => {}
        }
    }

    /// Try again after a failure: the page that failed.
    fn retry(&self, index: i32) {
        let Some(tab) = Tab::from_index(index) else {
            return;
        };
        let page = match tab {
            Tab::Files => 1,
            Tab::Changelog => self.changes_page.get() + 1,
            Tab::Comments => self.comments_page.get() + 1,
        };
        self.fetch(tab, page);
    }

    fn fetch(&self, tab: Tab, page: u32) {
        let Some((remote_id, audio)) = self.target.borrow().clone() else {
            return;
        };
        self.set_load(tab, TabLoad::Loading);
        let generation = self.generation.get();
        let client = self.client.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-detail-tab",
            move || -> Result<Fetched, String> {
                let err = |e: shiv_api::ApiError| e.to_string();
                match tab {
                    Tab::Files => client
                        .files(&remote_id, audio)
                        .map(Fetched::Files)
                        .map_err(err),
                    Tab::Changelog => client
                        .changelog(&remote_id, audio, page)
                        .map(Fetched::Changes)
                        .map_err(err),
                    Tab::Comments => client
                        .comments(&remote_id, audio, page)
                        .map(Fetched::Comments)
                        .map_err(err),
                }
            },
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_tabs(|t| t.loaded(generation, tab, page, result));
                });
            },
        );
        if !started {
            self.set_load(tab, TabLoad::Failed);
        }
    }

    fn loaded(&self, generation: u64, tab: Tab, page: u32, result: Result<Fetched, String>) {
        if generation != self.generation.get() {
            return;
        }
        let now = now_epoch();
        let fetched = match result {
            Ok(f) => f,
            Err(_) => return self.set_load(tab, TabLoad::Failed),
        };
        match fetched {
            Fetched::Files(files) => self.files.set_vec(file_rows(&files, now)),
            Fetched::Changes(p) => {
                append(&self.changes, change_rows(&p.items, now), page == 1);
                self.changes_page.set(page);
                self.with_state(|s| s.set_changes_more(!p.complete && !p.items.is_empty()));
            }
            Fetched::Comments(Comments::Hidden) => {
                return self.set_load(Tab::Comments, TabLoad::Hidden);
            }
            Fetched::Comments(Comments::Page(p)) => {
                append(&self.comments, comment_rows(&p.items, now), page == 1);
                self.comments_page.set(page);
                self.with_state(|s| s.set_comments_more(!p.complete && !p.items.is_empty()));
            }
        }
        self.set_load(tab, TabLoad::Ready);
    }
}

fn append(model: &VecModel<TextRow>, rows: Vec<TextRow>, replace: bool) {
    if replace {
        model.set_vec(rows);
    } else {
        for row in rows {
            model.push(row);
        }
    }
}

// ---------------------------------------------------------------------------
// Rows (pure, unit-tested)

fn when(secs: Option<i64>, now: i64) -> Option<String> {
    secs.map(|s| relative_secs(s, now))
}

fn join(parts: impl IntoIterator<Item = Option<String>>) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

fn plain(html: &str) -> String {
    let text = crate::describe::to_plain_text(html);
    match text.char_indices().nth(MAX_BODY) {
        Some((cut, _)) => format!("{}\u{2026}", text[..cut].trim_end()),
        None => text,
    }
}

fn file_rows(files: &[ModFile], now: i64) -> Vec<TextRow> {
    files
        .iter()
        .map(|f| TextRow {
            title: f.name.as_str().into(),
            meta: join([
                Some(fmt_size(Some(f.size))),
                Some(format!("{} downloads", fmt_count(f.downloads))),
                when(f.added, now),
            ])
            .into(),
            body: Default::default(),
        })
        .collect()
}

fn change_rows(entries: &[ChangelogEntry], now: i64) -> Vec<TextRow> {
    entries
        .iter()
        .map(|e| {
            let title = match (e.title.trim(), e.version.as_deref()) {
                ("", Some(v)) => format!("Version {v}"),
                ("", None) => "Update".to_owned(),
                (t, _) => t.to_owned(),
            };
            let version = e
                .version
                .as_deref()
                .filter(|_| !e.title.trim().is_empty())
                .map(|v| format!("Version {v}"));
            let mut body: Vec<String> = e
                .changes
                .iter()
                .map(|(cat, text)| {
                    if cat.is_empty() {
                        format!("\u{2022} {text}")
                    } else {
                        format!("\u{2022} {cat}: {text}")
                    }
                })
                .collect();
            let text = plain(&e.text);
            if !text.is_empty() {
                body.push(text);
            }
            TextRow {
                title: title.into(),
                meta: join([version, when(e.added, now)]).into(),
                body: body.join("\n").into(),
            }
        })
        .collect()
}

fn comment_rows(comments: &[Comment], now: i64) -> Vec<TextRow> {
    comments
        .iter()
        .map(|c| {
            let author = if c.author.trim().is_empty() {
                "Anonymous"
            } else {
                c.author.trim()
            };
            TextRow {
                title: author.into(),
                meta: join([
                    c.pinned.then(|| "Pinned".to_owned()),
                    when(c.added, now),
                    (c.replies > 0).then(|| {
                        format!(
                            "{} {}",
                            c.replies,
                            if c.replies == 1 { "reply" } else { "replies" }
                        )
                    }),
                ])
                .into(),
                body: plain(&c.text).into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_show_size_downloads_and_age() {
        let rows = file_rows(
            &[ModFile {
                name: "a.zip".into(),
                size: 2_500_000,
                added: Some(1_000),
                downloads: 1_234,
            }],
            1_000 + 5 * 86_400,
        );
        assert_eq!(rows[0].title.as_str(), "a.zip");
        assert_eq!(
            rows[0].meta.as_str(),
            "2.5 MB \u{b7} 1,234 downloads \u{b7} 5 days ago"
        );
    }

    #[test]
    fn changelog_lists_changes_and_versions() {
        let e = ChangelogEntry {
            title: "Fix".into(),
            version: Some("1.2".into()),
            added: Some(0),
            changes: vec![
                ("Bugfix".into(), "Fixed it".into()),
                (String::new(), "Plain".into()),
            ],
            text: "<p>More <b>words</b></p>".into(),
        };
        let rows = change_rows(&[e], 120);
        assert_eq!(rows[0].title.as_str(), "Fix");
        assert_eq!(rows[0].meta.as_str(), "Version 1.2 \u{b7} 2 minutes ago");
        assert_eq!(
            rows[0].body.as_str(),
            "\u{2022} Bugfix: Fixed it\n\u{2022} Plain\nMore words"
        );
        let bare = ChangelogEntry {
            title: String::new(),
            version: Some("2".into()),
            added: None,
            changes: vec![],
            text: String::new(),
        };
        assert_eq!(change_rows(&[bare], 0)[0].title.as_str(), "Version 2");
    }

    #[test]
    fn comments_are_plain_text_with_author_and_replies() {
        let c = |author: &str, replies: u64, pinned: bool| Comment {
            author: author.into(),
            added: Some(0),
            replies,
            pinned,
            text: "<p>Hi<br>there &amp; <script>x()</script>bye</p>".into(),
        };
        let rows = comment_rows(&[c("Ann", 1, true), c(" ", 3, false)], 86_400);
        assert_eq!(rows[0].title.as_str(), "Ann");
        assert_eq!(
            rows[0].meta.as_str(),
            "Pinned \u{b7} 1 day ago \u{b7} 1 reply"
        );
        assert_eq!(rows[1].title.as_str(), "Anonymous");
        assert!(rows[1].meta.ends_with("3 replies"));
        assert!(!rows[0].body.contains('<') && rows[0].body.contains("there & "));
        assert!(!rows[0].body.contains("x()"));
    }

    #[test]
    fn long_bodies_are_cut() {
        let long = "word ".repeat(2_000);
        let out = plain(&long);
        assert!(out.chars().count() <= MAX_BODY + 1);
        assert!(out.ends_with('\u{2026}'));
    }
}
