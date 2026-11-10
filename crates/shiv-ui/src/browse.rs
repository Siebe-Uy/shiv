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
use crate::{
    App, BrowseState, DownloadRow, GalleryThumb, HeroOption, ImageState, InstallState, LoadState,
    ModDetail, ModRow,
};

const SORTS: [(&str, SortKey); 5] = [
    ("Newest", SortKey::Newest),
    ("Last updated", SortKey::Updated),
    ("Most downloaded", SortKey::MostDownloaded),
    ("Most liked", SortKey::MostLiked),
    ("Name", SortKey::Name),
];
const ALL_CATEGORIES: &str = "All categories";
const ALL_HEROES: &str = "All heroes";
/// Decoded thumbnails kept in memory (about 190 at 240x135 RGBA).
const THUMB_BUDGET: usize = 24 * 1024 * 1024;
const THUMB_WORKERS: usize = 4;
/// Pending thumbnail requests; older ones (scrolled past) are dropped first.
const THUMB_QUEUE: usize = 48;
/// Blurred NSFW thumbnails kept (about 32 KB each).
const OBSCURED_KEEP: usize = 128;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
const GAMEBANANA_SS: &str = "https://images.gamebanana.com/img/ss/mods/";
/// Gallery images are decoded to fit this box (aspect kept).
pub(crate) const GALLERY_BOX: (u32, u32) = (960, 540);
/// Decoded gallery images kept while a detail page is open: the image shown
/// and its two neighbours, plus one (about 2 MB each). Emptied on Back.
const GALLERY_BUDGET: usize = 4 * (GALLERY_BOX.0 * GALLERY_BOX.1 * 4) as usize;
const GALLERY_WORKERS: usize = 2;
/// Pending gallery requests; stepping quickly drops the ones passed.
const GALLERY_QUEUE: usize = 3;
/// Most images shown in one gallery.
const GALLERY_MAX: usize = 24;

/// Serialises installs: two installs must not allocate pak numbers at once.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// The controller, reachable from closures posted by worker threads.
    static BROWSE: RefCell<Option<Rc<Browse>>> = const { RefCell::new(None) };
}

/// Show a mod's detail page from another screen (`from` is where Back goes).
pub(crate) fn show_detail_from(id: &str, from: i32) -> bool {
    let mut shown = false;
    with_browse(|b| shown = b.show_detail_from(id, from));
    shown
}

fn with_browse(f: impl FnOnce(&Rc<Browse>)) {
    if let Some(b) = BROWSE.with(|cell| cell.borrow().clone()) {
        f(&b);
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers (pure, unit-tested)

/// `1234567` -> `1,234,567`.
pub(crate) fn fmt_count(n: u64) -> String {
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
pub(crate) fn fmt_size(bytes: Option<u64>) -> String {
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

/// Seconds since the Unix epoch for an ISO-8601 UTC timestamp (`Z` and
/// fractional seconds optional; a bare date means midnight).
fn iso_to_epoch(iso: &str) -> Option<i64> {
    let date = short_date(iso)?;
    let num = |r: std::ops::Range<usize>| date.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let time = iso.get(11..).unwrap_or_default();
    let part = |r: std::ops::Range<usize>| {
        time.get(r)
            .and_then(|t| t.parse::<i64>().ok())
            .filter(|n| (0..60).contains(n))
    };
    let secs = if time.len() >= 8 {
        part(0..2)? * 3600 + part(3..5)? * 60 + part(6..8)?
    } else {
        0
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + secs)
}

/// "5 days ago" style text for a timestamp, relative to `now` (epoch
/// seconds). Anything in the future reads as "just now".
pub(crate) fn relative_time(iso: &str, now: i64) -> Option<String> {
    Some(relative_secs(iso_to_epoch(iso)?, now))
}

/// [`relative_time`] for a Unix timestamp in seconds.
pub(crate) fn relative_secs(then: i64, now: i64) -> String {
    let secs = now - then;
    let (n, unit) = match secs {
        ..60 => return "just now".to_owned(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        86_400..2_592_000 => (secs / 86_400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub(crate) fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
    Some(small_variant(raw))
}

/// The 530 px variant of a GameBanana screenshot; other URLs unchanged.
fn small_variant(raw: &str) -> String {
    if let Some(file) = raw.strip_prefix(GAMEBANANA_SS)
        && !file.is_empty()
        && !file.contains('/')
        && !file.contains("-90_")
    {
        return format!("{GAMEBANANA_SS}530-90_{file}");
    }
    raw.to_owned()
}

/// One gallery image: the full picture for the large view and the small
/// variant for the strip (shared with the list's thumbnail cache).
#[derive(Debug, Clone, PartialEq)]
struct GalleryImage {
    full: String,
    small: String,
}

impl GalleryImage {
    /// The strip's source: the small variant, or the full image when the
    /// small one failed (GameBanana only generates it for some screenshots).
    fn strip_url(&self, cache: &thumbs::Cache) -> &str {
        if cache.has_failed(&self.small) {
            &self.full
        } else {
            &self.small
        }
    }
}

/// The gallery for a mod: its `images` (web URLs only, duplicates dropped, at
/// most `GALLERY_MAX`), else its thumbnail.
fn gallery_images(m: &ModDto) -> Vec<GalleryImage> {
    let web = |u: &&String| u.starts_with("https://") || u.starts_with("http://");
    let mut seen = HashSet::new();
    let mut out: Vec<GalleryImage> = m
        .images
        .iter()
        .filter(web)
        .filter(|u| seen.insert(u.as_str()))
        .take(GALLERY_MAX)
        .map(|u| GalleryImage {
            full: u.clone(),
            small: small_variant(u),
        })
        .collect();
    if out.is_empty()
        && let Some(t) = m.thumbnail_url.as_ref().filter(|u| web(u))
    {
        out.push(GalleryImage {
            full: t.clone(),
            small: small_variant(t),
        });
    }
    out
}

/// The detail page's text, from the catalog entry.
fn detail_info(m: &ModDto) -> ModDetail {
    detail_info_at(m, now_epoch())
}

fn detail_info_at(m: &ModDto, now: i64) -> ModDetail {
    fn non_empty(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    let kind: Vec<&str> = [non_empty(&m.category), non_empty(&m.hero)]
        .into_iter()
        .flatten()
        .collect();
    let tags: Vec<&str> = m
        .tags
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    let date = |s: Option<&str>| s.and_then(short_date).unwrap_or_default().into();
    ModDetail {
        id: m.id.as_str().into(),
        name: m.name.as_str().into(),
        author: non_empty(&m.author).unwrap_or_default().into(),
        kind: kind.join(" \u{b7} ").into(),
        tags: tags.join(", ").into(),
        downloads: fmt_count(m.download_count).into(),
        likes: fmt_count(m.likes).into(),
        added: date(m.remote_added_at.as_deref()),
        updated: m
            .last_updated()
            .and_then(|u| relative_time(u, now))
            .unwrap_or_default()
            .into(),
        description: m
            .description
            .as_deref()
            .map(crate::describe::to_plain_text)
            .unwrap_or_default()
            .into(),
        has_link: m
            .remote_url
            .as_deref()
            .is_some_and(shiv_core::link::is_web_link),
        nsfw: m.is_nsfw,
        author_id: match (non_empty(&m.author), non_empty(&m.mod_author_id)) {
            (Some(_), Some(id)) => id.into(),
            _ => Default::default(),
        },
        donation_label: m
            .donation_link()
            .map(|(platform, _)| support_label(platform).into())
            .unwrap_or_default(),
    }
}

/// Button text for a support page: "Support on Ko-fi".
fn support_label(platform: &str) -> String {
    match platform {
        "Support the author" => platform.to_owned(),
        p => format!("Support on {p}"),
    }
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
/// Distinct heroes in the catalog, A to Z.
fn heroes(mods: &[ModDto]) -> Vec<String> {
    let mut list: Vec<String> = mods
        .iter()
        .filter_map(|m| m.hero.as_deref().map(str::trim).filter(|h| !h.is_empty()))
        .map(str::to_owned)
        .collect();
    list.sort_by_key(|h| h.to_lowercase());
    list.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    list
}

/// Text of the hero filter button.
fn heroes_label(selected: &[String]) -> String {
    match selected {
        [] => ALL_HEROES.to_owned(),
        [one] => one.clone(),
        many => format!("{} heroes", many.len()),
    }
}

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

/// Called on the event loop when a request finished: the URL, and whether
/// the image is now in the cache.
type OnLoaded = fn(&Browse, &str, bool);

/// A pool of workers fetching and decoding images into one cache, newest
/// request first.
#[derive(Clone)]
struct ThumbLoader {
    shared: Arc<(Mutex<ThumbQueue>, Condvar)>,
    queue: usize,
}

impl ThumbLoader {
    fn start(
        cache: Arc<thumbs::Cache>,
        weak: Weak<App>,
        name: &str,
        workers: usize,
        queue: usize,
        on_loaded: OnLoaded,
    ) -> Self {
        let loader = ThumbLoader {
            shared: Arc::new((Mutex::new(ThumbQueue::default()), Condvar::new())),
            queue,
        };
        for n in 0..workers {
            let shared = loader.shared.clone();
            let cache = cache.clone();
            let weak = weak.clone();
            crate::spawn_worker(
                &format!("{name}-{n}"),
                move || thumb_worker(&shared, &cache, &weak, on_loaded),
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
        if q.pending.len() > self.queue
            && let Some(old) = q.pending.pop_front()
        {
            q.known.remove(&old);
        }
        cvar.notify_one();
    }

    /// Forget requests not yet started (those in flight finish).
    fn clear_pending(&self) {
        let (lock, _) = &*self.shared;
        let mut q = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let ThumbQueue { pending, known } = &mut *q;
        for url in pending.drain(..) {
            known.remove(&url);
        }
    }
}

fn thumb_worker(
    shared: &(Mutex<ThumbQueue>, Condvar),
    cache: &thumbs::Cache,
    weak: &Weak<App>,
    on_loaded: OnLoaded,
) {
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
        // Failures are remembered by the cache.
        let posted = weak.upgrade_in_event_loop(move |_| {
            with_browse(|b| on_loaded(b, &url, ok));
        });
        if posted.is_err() {
            return; // event loop is gone
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
    /// "Updated 2 days ago", or empty when the catalog has no date.
    updated: SharedString,
    thumb_url: Option<String>,
    version: Option<String>,
    downloadable: bool,
    nsfw: bool,
}

impl Entry {
    fn from_dto(m: ModDto) -> Self {
        Self::from_dto_at(m, now_epoch())
    }

    fn from_dto_at(m: ModDto, now: i64) -> Self {
        let byline = match (m.author.as_deref(), m.category.as_deref()) {
            (Some(a), Some(c)) => format!("{a} \u{b7} {c}"),
            (Some(a), None) => a.to_owned(),
            (None, Some(c)) => c.to_owned(),
            (None, None) => String::new(),
        };
        let stats = format!(
            "{} downloads \u{b7} {} likes",
            fmt_count(m.download_count),
            fmt_count(m.likes)
        );
        let updated = m
            .last_updated()
            .and_then(|u| relative_time(u, now))
            .map(|d| format!("Updated {d}"))
            .unwrap_or_default();
        Entry {
            thumb_url: thumb_url(&m),
            version: m.files_updated_at.or(m.remote_updated_at),
            downloadable: m.downloadable,
            nsfw: m.is_nsfw,
            byline: byline.into(),
            stats: stats.into(),
            updated: updated.into(),
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
    /// The mod whose detail page is open (it may be filtered out of the list).
    detail: RefCell<Option<Entry>>,
    /// Change notifications for [`DetailRow`].
    detail_notify: ModelNotify,
}

impl ModList {
    fn set_entries(&self, entries: Vec<Entry>) {
        *self.entries.borrow_mut() = entries;
        self.notify.reset();
    }

    /// The list entry for `id`, else the open detail page's (installs keep
    /// working for a mod a new search filtered out).
    fn find(&self, id: &str) -> Option<Entry> {
        let listed = self.entries.borrow().iter().find(|e| e.id == id).cloned();
        listed.or_else(|| {
            self.detail
                .borrow()
                .as_ref()
                .filter(|e| e.id == id)
                .cloned()
        })
    }

    fn changed(&self, id: &str) {
        let idx = self.entries.borrow().iter().position(|e| e.id == id);
        if let Some(i) = idx {
            self.notify.row_changed(i);
        }
        if self.detail.borrow().as_ref().is_some_and(|e| e.id == id) {
            self.detail_notify.row_changed(0);
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

impl ModList {
    /// A row as shown, with its install state. `with_thumb` also resolves
    /// (or requests) the thumbnail.
    fn row_for(&self, e: &Entry, with_thumb: bool) -> ModRow {
        let mut out = ModRow {
            id: e.id.as_str().into(),
            name: e.name.as_str().into(),
            byline: e.byline.clone(),
            stats: e.stats.clone(),
            updated: e.updated.clone(),
            nsfw: e.nsfw,
            ..ModRow::default()
        };
        if with_thumb && let Some(url) = &e.thumb_url {
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
            return out;
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
        out
    }
}

impl Model for ModList {
    type Data = ModRow;

    fn row_count(&self) -> usize {
        self.entries.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<ModRow> {
        let entries = self.entries.borrow();
        Some(self.row_for(entries.get(row)?, true))
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

/// The open detail page's mod as one row, computed by the same code and
/// from the same install state as the list, so both always agree.
struct DetailRow(Rc<ModList>);

impl Model for DetailRow {
    type Data = ModRow;

    fn row_count(&self) -> usize {
        usize::from(self.0.detail.borrow().is_some())
    }

    fn row_data(&self, row: usize) -> Option<ModRow> {
        let detail = self.0.detail.borrow();
        let e = detail.as_ref().filter(|_| row == 0)?;
        Some(self.0.row_for(e, false))
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.0.detail_notify
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
    nsfw: bool,
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
        nsfw: task.nsfw,
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

/// Whether NSFW images are shown unblurred (the Browse switch; shared with
/// Installed). False before Browse is attached.
pub(crate) fn show_nsfw() -> bool {
    let mut out = false;
    with_browse(|b| out = b.list.show_nsfw.get());
    out
}

/// Whether the loaded catalog marks `id` NSFW; `None` when no catalog has
/// loaded (or Browse is not attached).
pub(crate) fn catalog_nsfw(id: &str) -> Option<bool> {
    let mut out = None;
    with_browse(|b| out = b.nsfw_ids.borrow().as_ref().map(|ids| ids.contains(id)));
    out
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
    /// Ids the loaded catalog marks NSFW; `None` until it has loaded.
    nsfw_ids: RefCell<Option<HashSet<String>>>,
    categories: RefCell<Vec<String>>,
    /// Heroes in the catalog, in the order of `hero_model`.
    heroes: RefCell<Vec<String>>,
    hero_model: Rc<VecModel<HeroOption>>,
    query: RefCell<ModQuery>,
    load_generation: Cell<u64>,
    installed_generation: Cell<u64>,
    next_job: Cell<u64>,
    /// The open download picker: the mod and its files.
    picker: RefCell<Option<(String, Vec<DownloadFile>)>>,
    search_timer: Timer,
    this: RefCell<RcWeak<Browse>>,
    /// The open detail page's gallery and link.
    detail: RefCell<Option<Detail>>,
    gallery_cache: Arc<thumbs::Cache>,
    gallery: ThumbLoader,
    /// Opens a web link (blocking, on a worker); replaced in tests.
    opener: RefCell<Opener>,
}

type Opener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// State of the open detail page beyond its text (which lives in Slint).
struct Detail {
    nsfw: bool,
    images: Vec<GalleryImage>,
    index: usize,
    /// The mod's page, when it is an https link.
    link: Option<String>,
    /// The author's support page, when it is on a known host.
    donation: Option<String>,
    /// Strip thumbnails, one per image.
    thumbs: Rc<VecModel<GalleryThumb>>,
    /// Blurred large images (NSFW with Show NSFW off), by URL.
    obscured: HashMap<String, slint::Image>,
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
        loader: ThumbLoader::start(
            cache.clone(),
            weak.clone(),
            "shiv-thumb",
            THUMB_WORKERS,
            THUMB_QUEUE,
            |b, url, ok| {
                if ok {
                    b.list.thumb_ready(url);
                    b.strip_ready(url);
                }
            },
        ),
        cache: cache.clone(),
        show_nsfw: Cell::new(false),
        obscured: RefCell::new(HashMap::new()),
        notify: ModelNotify::default(),
        detail: RefCell::new(None),
        detail_notify: ModelNotify::default(),
    });
    let gallery_cache = Arc::new(thumbs::Cache::fetching(
        GALLERY_BUDGET,
        thumbs::Shape::Fit(GALLERY_BOX.0, GALLERY_BOX.1),
    ));
    let browse = Rc::new(Browse {
        weak: weak.clone(),
        client: Arc::new(Client::new(api_url)),
        list: list.clone(),
        nsfw_ids: RefCell::new(None),
        catalog: RefCell::new(Rc::new(Vec::new())),
        categories: RefCell::new(Vec::new()),
        heroes: RefCell::new(Vec::new()),
        hero_model: Rc::new(VecModel::default()),
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
        detail: RefCell::new(None),
        gallery: ThumbLoader::start(
            gallery_cache.clone(),
            weak.clone(),
            "shiv-gallery",
            GALLERY_WORKERS,
            GALLERY_QUEUE,
            |b, url, ok| b.gallery_loaded(url, ok),
        ),
        gallery_cache,
        opener: RefCell::new(Arc::new(|url: &str| {
            shiv_core::link::open_web_link(url).map_err(|e| e.to_string())
        })),
    });
    *browse.this.borrow_mut() = Rc::downgrade(&browse);
    BROWSE.with(|cell| *cell.borrow_mut() = Some(browse.clone()));

    let state = app.global::<BrowseState>();
    let sorts: Vec<SharedString> = SORTS.iter().map(|(label, _)| (*label).into()).collect();
    state.set_sorts(ModelRc::new(VecModel::from(sorts)));
    state.set_categories(ModelRc::new(VecModel::from(vec![SharedString::from(
        ALL_CATEGORIES,
    )])));
    state.set_mods(ModelRc::from(list.clone()));
    state.set_heroes(ModelRc::from(browse.hero_model.clone()));
    crate::detail_tabs::attach(app);
    state.set_detail_row(ModelRc::new(DetailRow(list)));

    let handle = |f: fn(&Rc<Browse>)| move || with_browse(f);
    state.on_search_edited(|text| with_browse(|b| b.search_edited(text.as_str())));
    state.on_sort_selected(|i| with_browse(|b| b.sort_selected(i)));
    state.on_category_selected(|i| with_browse(|b| b.category_selected(i)));
    state.on_author_selected(|| with_browse(|b| b.author_selected()));
    state.on_author_filter_cleared(|| with_browse(|b| b.author_filter_cleared()));
    state.on_open_donation(|| with_browse(|b| b.open_donation()));
    state.on_hero_toggled(|i| with_browse(|b| b.hero_toggled(i)));
    state.on_heroes_cleared(|| with_browse(|b| b.heroes_cleared()));
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
            b.detail_images_changed();
        });
        crate::installed::nsfw_changed();
    });
    state.on_open_detail(|id| with_browse(|b| b.open_detail(id.as_str())));
    state.on_close_detail(handle(|b| b.close_detail()));
    state.on_gallery_select(|i| {
        with_browse(|b| {
            if let Ok(i) = usize::try_from(i) {
                b.show_image(i);
            }
        });
    });
    state.on_gallery_step(|delta| with_browse(|b| b.gallery_step(delta)));
    state.on_open_link(handle(|b| b.open_link()));

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
        if self.list.detail.borrow().is_some() {
            self.list.detail_notify.row_changed(0);
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
        let hero_list = heroes(&mods);
        self.query
            .borrow_mut()
            .heroes
            .retain(|s| hero_list.iter().any(|h| h.eq_ignore_ascii_case(s)));
        *self.heroes.borrow_mut() = hero_list;
        self.sync_heroes();
        *self.nsfw_ids.borrow_mut() = Some(
            mods.iter()
                .filter(|m| m.is_nsfw)
                .map(|m| m.id.clone())
                .collect(),
        );
        *self.catalog.borrow_mut() = Rc::new(mods);
        *self.list.installed.borrow_mut() = installed;
        // Installed rows of mods installed before Shiv recorded the flag.
        crate::installed::nsfw_changed();
        self.with_state(|s| {
            s.set_categories(ModelRc::new(VecModel::from(labels)));
            s.set_category_index(index as i32);
            s.set_load_state(LoadState::Ready);
        });
        self.refresh();
        #[cfg(any(debug_assertions, feature = "dev-probe"))]
        dev_probe::catalog_loaded();
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
        // New results start at the top.
        self.with_state(|s| {
            s.set_summary(summary.into());
            s.set_list_scroll(0.0);
        });
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

    /// Push the hero list and the selection to the UI.
    fn sync_heroes(&self) {
        let list = self.heroes.borrow();
        let query = self.query.borrow();
        let rows: Vec<HeroOption> = list
            .iter()
            .map(|h| HeroOption {
                name: h.as_str().into(),
                selected: query.heroes.iter().any(|s| s.eq_ignore_ascii_case(h)),
            })
            .collect();
        self.hero_model.set_vec(rows);
        let label = heroes_label(&query.heroes);
        drop(query);
        drop(list);
        self.with_state(|s| s.set_heroes_label(label.as_str().into()));
    }

    fn hero_toggled(&self, index: i32) {
        let Some(hero) = usize::try_from(index)
            .ok()
            .and_then(|i| self.heroes.borrow().get(i).cloned())
        else {
            return;
        };
        {
            let mut q = self.query.borrow_mut();
            match q.heroes.iter().position(|s| s.eq_ignore_ascii_case(&hero)) {
                Some(p) => {
                    q.heroes.remove(p);
                }
                None => q.heroes.push(hero),
            }
        }
        self.sync_heroes();
        self.refresh();
    }

    fn heroes_cleared(&self) {
        self.query.borrow_mut().heroes.clear();
        self.sync_heroes();
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
        let Some(entry) = self.list.find(id) else {
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
        let name = self.list.find(id).map_or_else(|| id.to_owned(), |e| e.name);
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
        let Some(entry) = self.list.find(id) else {
            // The catalog was reloaded or filtered meanwhile.
            self.set_banner(
                "Could not install: that mod is no longer in the list. Find it again and retry.",
            );
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
            nsfw: entry.nsfw,
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
                    let name = self.list.find(id).map_or_else(|| id.to_owned(), |e| e.name);
                    self.set_banner(&format!("Could not install {name}. {text}"));
                }
            }
        }
        self.list.changed(id);
    }

    // -----------------------------------------------------------------------
    // Detail page

    /// Open the detail page for `id` (from the loaded catalog; no network
    /// needed for the text). The list stays as it is underneath.
    fn open_detail(&self, id: &str) {
        let catalog = self.catalog.borrow().clone();
        let Some(m) = catalog.iter().find(|m| m.id == id) else {
            return;
        };
        self.drop_gallery();
        let entry = self
            .list
            .find(id)
            .unwrap_or_else(|| Entry::from_dto(m.clone()));
        *self.list.detail.borrow_mut() = Some(entry);
        self.list.detail_notify.reset();
        let images = gallery_images(m);
        let thumbs = Rc::new(VecModel::from(vec![GalleryThumb::default(); images.len()]));
        let count = images.len() as i32;
        *self.detail.borrow_mut() = Some(Detail {
            nsfw: m.is_nsfw,
            images,
            index: 0,
            link: m
                .remote_url
                .clone()
                .filter(|u| shiv_core::link::is_web_link(u)),
            donation: m.donation_link().map(|(_, url)| url.to_owned()),
            thumbs: thumbs.clone(),
            obscured: HashMap::new(),
        });
        crate::detail_tabs::open(&m.remote_id, m.is_audio);
        let info = detail_info(m);
        self.with_state(|s| {
            s.set_detail(info);
            s.set_gallery_count(count);
            s.set_gallery_thumbs(ModelRc::from(thumbs));
            s.set_detail_open(true);
        });
        self.refresh_strip();
        self.show_image(0);
    }

    /// Open a mod's page from another screen; Back returns to `from`.
    /// False when the catalog has no such mod (not loaded, or removed).
    fn show_detail_from(&self, id: &str, from: i32) -> bool {
        if !self.catalog.borrow().iter().any(|m| m.id == id) {
            return false;
        }
        self.open_detail(id);
        if let Some(app) = self.weak.upgrade() {
            app.global::<BrowseState>().set_return_screen(from);
            app.set_screen(0);
        }
        true
    }

    /// Back to the list (or the screen that opened the page): the page and
    /// every image it decoded are dropped.
    fn close_detail(&self) {
        let mut back = -1;
        self.with_state(|s| {
            s.set_detail_open(false);
            back = s.get_return_screen();
            s.set_return_screen(-1);
        });
        if back >= 0
            && let Some(app) = self.weak.upgrade()
        {
            app.set_screen(back);
        }
        self.drop_gallery();
        crate::release_freed_memory();
    }

    fn drop_gallery(&self) {
        crate::detail_tabs::close();
        let open = self.detail.borrow_mut().take().is_some();
        *self.list.detail.borrow_mut() = None;
        self.list.detail_notify.reset();
        self.gallery.clear_pending();
        self.gallery_cache.clear();
        if open {
            self.with_state(|s| {
                s.set_gallery_image(slint::Image::default());
                s.set_gallery_state(ImageState::None);
                s.set_gallery_thumbs(ModelRc::default());
                s.set_gallery_count(0);
                s.set_gallery_index(0);
                s.set_detail(ModDetail::default());
            });
        }
    }

    /// A large image as shown: blurred for an NSFW mod unless Show NSFW is
    /// on. `None` when the blurred copy could not be made (never the original).
    fn display_large(&self, url: &str, img: &thumbs::Image) -> Option<slint::Image> {
        let mut detail = self.detail.borrow_mut();
        let d = detail.as_mut()?;
        if !d.nsfw || self.list.show_nsfw.get() {
            return Some(to_slint(img));
        }
        if let Some(done) = d.obscured.get(url) {
            return Some(done.clone());
        }
        let blurred = to_slint(&thumbs::obscured_any(img)?);
        d.obscured.insert(url.to_owned(), blurred.clone());
        Some(blurred)
    }

    /// Show gallery image `index`: from the cache, else load it (and its
    /// neighbours) on the gallery workers.
    fn show_image(&self, index: usize) {
        let (url, neighbours) = {
            let mut detail = self.detail.borrow_mut();
            let Some(d) = detail.as_mut() else { return };
            let n = d.images.len();
            if index >= n {
                return;
            }
            d.index = index;
            let near = [(index + 1) % n, (index + n - 1) % n]
                .into_iter()
                .filter(|i| *i != index)
                .map(|i| d.images[i].full.clone())
                .collect::<Vec<_>>();
            (d.images[index].full.clone(), near)
        };
        let shown = self
            .gallery_cache
            .get(&url)
            .map(|img| self.display_large(&url, &img));
        let (image, state) = match shown {
            Some(Some(image)) => (image, ImageState::Ready),
            Some(None) => (slint::Image::default(), ImageState::Failed),
            None if self.gallery_cache.has_failed(&url) => {
                (slint::Image::default(), ImageState::Failed)
            }
            None => (slint::Image::default(), ImageState::Loading),
        };
        // Newest request first: the neighbours, then the image itself.
        self.gallery.clear_pending();
        for near in neighbours.iter().chain([&url]) {
            if self.gallery_cache.get(near).is_none() && !self.gallery_cache.has_failed(near) {
                self.gallery.request(near);
            }
        }
        self.with_state(|s| {
            s.set_gallery_index(index as i32);
            s.set_gallery_image(image);
            s.set_gallery_state(state);
        });
    }

    fn gallery_step(&self, delta: i32) {
        let next = {
            let detail = self.detail.borrow();
            let Some(d) = detail.as_ref().filter(|d| !d.images.is_empty()) else {
                return;
            };
            let n = d.images.len() as i64;
            (d.index as i64 + i64::from(delta)).rem_euclid(n) as usize
        };
        self.show_image(next);
    }

    /// A gallery worker finished `url`. Results for a closed page or another
    /// mod are dropped from the cache at once.
    fn gallery_loaded(&self, url: &str, ok: bool) {
        let current = {
            let detail = self.detail.borrow();
            let Some(d) = detail.as_ref() else {
                self.gallery_cache.remove(url);
                return;
            };
            if !d.images.iter().any(|g| g.full == url) {
                self.gallery_cache.remove(url);
                return;
            }
            d.images.get(d.index).is_some_and(|g| g.full == url)
        };
        if current {
            let index = self.detail.borrow().as_ref().map_or(0, |d| d.index);
            if ok {
                self.show_image(index);
            } else {
                self.with_state(|s| s.set_gallery_state(ImageState::Failed));
            }
        }
    }

    /// Fill the thumbnail strip from the list's thumbnail cache (blurred
    /// like the list for NSFW mods), requesting what is missing.
    fn refresh_strip(&self) {
        let detail = self.detail.borrow();
        let Some(d) = detail.as_ref() else { return };
        for (i, g) in d.images.iter().enumerate() {
            let url = g.strip_url(&self.list.cache);
            let shown = match self.list.cache.get(url) {
                Some(img) => self.list.display_thumb(url, &img, d.nsfw),
                None => {
                    if !self.list.cache.has_failed(url) {
                        self.list.loader.request(url);
                    }
                    None
                }
            };
            let row = match shown {
                Some(image) => GalleryThumb {
                    image,
                    has_image: true,
                },
                None => GalleryThumb::default(),
            };
            d.thumbs.set_row_data(i, row);
        }
    }

    fn strip_ready(&self, url: &str) {
        let wanted = self
            .detail
            .borrow()
            .as_ref()
            .is_some_and(|d| d.images.iter().any(|g| g.small == url || g.full == url));
        if wanted {
            self.refresh_strip();
        }
    }

    /// Show NSFW changed: re-render the open page's images.
    fn detail_images_changed(&self) {
        let index = {
            let mut detail = self.detail.borrow_mut();
            let Some(d) = detail.as_mut() else { return };
            d.obscured.clear();
            d.index
        };
        self.refresh_strip();
        self.show_image(index);
    }

    /// Limit the list to the open mod's author and leave the detail page.
    fn author_selected(&self) {
        let (id, name) = {
            let detail = self.list.detail.borrow();
            let Some(entry) = detail.as_ref() else { return };
            let catalog = self.catalog.borrow();
            let Some(m) = catalog.iter().find(|m| m.id == entry.id) else {
                return;
            };
            let (Some(id), Some(name)) = (m.mod_author_id.clone(), m.author.clone()) else {
                return;
            };
            (id, name)
        };
        self.query.borrow_mut().author_id = Some(id);
        self.with_state(|s| {
            s.set_author_filter(name.as_str().into());
            s.set_return_screen(-1);
        });
        self.close_detail();
        self.refresh();
    }

    fn author_filter_cleared(&self) {
        self.query.borrow_mut().author_id = None;
        self.with_state(|s| s.set_author_filter("".into()));
        self.refresh();
    }

    fn open_link(&self) {
        let url = self.detail.borrow().as_ref().and_then(|d| d.link.clone());
        self.open_external(url);
    }

    fn open_donation(&self) {
        let url = self
            .detail
            .borrow()
            .as_ref()
            .and_then(|d| d.donation.clone());
        self.open_external(url);
    }

    /// Open a vetted https link in the system browser, off the UI thread.
    fn open_external(&self, url: Option<String>) {
        let Some(url) = url else {
            return;
        };
        let opener = self.opener.borrow().clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-open-link",
            move || opener(&url),
            move |result| {
                if let Err(text) = result.and_then(|r| r) {
                    let _ = weak.upgrade_in_event_loop(move |_| {
                        with_browse(|b| b.set_banner(&format!("Could not open the link. {text}")));
                    });
                }
            },
        );
        if !started {
            self.set_banner("Could not open the link. Could not start a background thread.");
        }
    }
}

/// Developer probe, for screenshots and memory measurements without pointer
/// automation. Debug builds, or release builds made with `--features
/// dev-probe`; a normal release ignores these variables. Once the catalog has
/// loaded: `SHIV_OPEN_MOD=<id>` opens that mod's detail page (after
/// `SHIV_OPEN_AFTER_SECS`, default 0), `SHIV_GALLERY_STEPS=<n>` then shows the
/// next image n times (one per second), and `SHIV_CLOSE_DETAIL_SECS=<s>` goes
/// Back s seconds after opening. `SHIV_DETAIL_TAB=<n>` picks a detail tab a
/// second after opening.
#[cfg(any(debug_assertions, feature = "dev-probe"))]
mod dev_probe {
    use super::*;

    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
        static TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    }

    fn after(secs: u64, f: impl FnOnce(&Browse) + 'static) {
        let timer = Timer::default();
        let mut f = Some(f);
        timer.start(
            TimerMode::SingleShot,
            Duration::from_secs(secs),
            move || {
                if let Some(f) = f.take() {
                    with_browse(|b| f(b));
                }
            },
        );
        TIMERS.with(|t| t.borrow_mut().push(timer));
    }

    pub(super) fn catalog_loaded() {
        if DONE.with(|d| d.replace(true)) {
            return;
        }
        let Some(id) = std::env::var("SHIV_OPEN_MOD")
            .ok()
            .filter(|v| !v.is_empty())
        else {
            return;
        };
        let num = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
        let open = num("SHIV_OPEN_AFTER_SECS").unwrap_or(0);
        let steps = num("SHIV_GALLERY_STEPS").unwrap_or(0);
        let close = num("SHIV_CLOSE_DETAIL_SECS");
        after(open, move |b| b.open_detail(&id));
        if let Some(tab) = num("SHIV_DETAIL_TAB") {
            after(open + 1, move |_| {
                crate::detail_tabs::select_for_probe(tab as i32);
            });
        }
        for n in 1..=steps {
            after(open + n, |b| b.gallery_step(1));
        }
        if let Some(secs) = close {
            after(open + secs, |b| b.close_detail());
        }
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
    /// What a catalog load does for NSFW: record the NSFW ids, tell Installed.
    pub fn catalog_loaded_with_nsfw(ids: &[&str]) {
        with_browse(|b| {
            *b.nsfw_ids.borrow_mut() = Some(ids.iter().map(|s| (*s).to_owned()).collect());
        });
        crate::installed::nsfw_changed();
    }

    /// The picker is open for a mod that has since left the list; pick a file.
    pub fn pick_for_vanished_mod() {
        with_browse(|b| {
            let file = DownloadFile {
                url: "http://127.0.0.1:9/x.zip".into(),
                name: "x.zip".into(),
                size: None,
                md5_checksum: None,
            };
            *b.picker.borrow_mut() = Some(("vanished".into(), vec![file]));
            b.pick_download(0);
        });
    }

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

    /// Decoded gallery bytes and images held.
    pub fn gallery_usage() -> (usize, usize) {
        let mut out = (0, 0);
        with_browse(|b| out = b.gallery_cache.usage());
        out
    }

    /// The mod whose detail page is open.
    pub fn detail_id() -> Option<String> {
        let mut out = None;
        with_browse(|b| out = b.list.detail.borrow().as_ref().map(|e| e.id.clone()));
        out
    }

    /// Replace the link opener (tests must never start a browser).
    pub fn set_opener(f: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static) {
        with_browse(|b| *b.opener.borrow_mut() = Arc::new(f));
    }

    /// A gallery worker delivering `url` late (after the page closed or
    /// moved on): fetched into the cache, then reported.
    pub fn late_gallery_result(url: &str) {
        with_browse(|b| {
            let ok = b.gallery_cache.get_or_fetch(url).is_some();
            b.gallery_loaded(url, ok);
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
    fn strip_falls_back_to_the_full_image_when_the_small_variant_fails() {
        let cache = thumbs::Cache::with_fetcher(1 << 20, |_| None);
        let g = GalleryImage {
            full: "https://images.gamebanana.com/img/ss/mods/a.jpg".into(),
            small: "https://images.gamebanana.com/img/ss/mods/530-90_a.jpg".into(),
        };
        assert_eq!(g.strip_url(&cache), g.small);
        assert!(cache.get_or_fetch(&g.small).is_none());
        assert_eq!(g.strip_url(&cache), g.full);
    }

    #[test]
    fn relative_time_reads_like_a_person_would() {
        let now = iso_to_epoch("2026-11-11T12:00:00.000Z").unwrap();
        let r = |iso: &str| relative_time(iso, now).unwrap();
        assert_eq!(r("2026-11-11T11:59:30Z"), "just now");
        assert_eq!(r("2026-11-11T11:59:00Z"), "1 minute ago");
        assert_eq!(r("2026-11-11T09:00:00Z"), "3 hours ago");
        assert_eq!(r("2026-11-06T12:00:00Z"), "5 days ago");
        assert_eq!(r("2026-10-10"), "1 month ago");
        assert_eq!(r("2024-11-11"), "2 years ago");
        assert_eq!(r("2027-01-01"), "just now");
        assert_eq!(relative_time("soon", now), None);
        assert_eq!(iso_to_epoch("1970-01-02T00:00:01Z"), Some(86_401));
    }

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
        let now = iso_to_epoch("2026-11-10T12:00:00Z").unwrap();
        let updated = |d| Entry::from_dto_at(d, now).updated.to_string();
        assert_eq!(
            updated(m(
                Some("2026-11-08T01:00:00Z"),
                Some("2025-01-01T00:00:00Z")
            )),
            "Updated 2 days ago"
        );
        assert_eq!(
            updated(m(None, Some("2025-01-01T00:00:00Z"))),
            "Updated 1 year ago"
        );
        assert_eq!(updated(m(None, None)), "");
        assert_eq!(
            Entry::from_dto(m(None, None)).stats.as_str(),
            "1,200 downloads \u{b7} 3 likes"
        );
        assert_eq!(SORTS[1], ("Last updated", SortKey::Updated));
    }

    #[test]
    fn gallery_lists_web_images_once_and_falls_back_to_the_thumbnail() {
        let gb = "https://images.gamebanana.com/img/ss/mods/6a0f.jpg";
        let m = ModDto {
            images: vec![
                gb.into(),
                "".into(),
                "file:///etc/passwd".into(),
                gb.into(),
                "http://x.test/b.png".into(),
            ],
            thumbnail_url: Some("https://x.test/t.png".into()),
            ..ModDto::default()
        };
        let g = gallery_images(&m);
        assert_eq!(
            g,
            [
                GalleryImage {
                    full: gb.into(),
                    small: "https://images.gamebanana.com/img/ss/mods/530-90_6a0f.jpg".into(),
                },
                GalleryImage {
                    full: "http://x.test/b.png".into(),
                    small: "http://x.test/b.png".into(),
                },
            ]
        );
        let only_thumb = ModDto {
            thumbnail_url: Some("https://x.test/t.png".into()),
            ..ModDto::default()
        };
        assert_eq!(gallery_images(&only_thumb).len(), 1);
        assert!(gallery_images(&ModDto::default()).is_empty());
        let many = ModDto {
            images: (0..100)
                .map(|i| format!("https://x.test/{i}.png"))
                .collect(),
            ..ModDto::default()
        };
        assert_eq!(gallery_images(&many).len(), GALLERY_MAX);
    }

    #[test]
    fn detail_text_comes_from_the_catalog_entry() {
        let m = ModDto {
            id: "m1".into(),
            name: "Mod".into(),
            author: Some(" ".into()),
            category: Some("Skins".into()),
            hero: Some("Haze".into()),
            tags: vec!["a".into(), "".into(), "b".into()],
            likes: 1500,
            remote_added_at: Some("2026-01-02T00:00:00Z".into()),
            files_updated_at: Some("2026-03-04T00:00:00Z".into()),
            description: Some("<p>Hi&nbsp;there</p>".into()),
            remote_url: Some("http://gamebanana.com/mods/1".into()),
            ..ModDto::default()
        };
        let now = iso_to_epoch("2026-03-10T00:00:00Z").unwrap();
        let d = detail_info_at(&m, now);
        assert_eq!(d.author.as_str(), "");
        assert_eq!(d.kind.as_str(), "Skins \u{b7} Haze");
        assert_eq!(d.tags.as_str(), "a, b");
        assert_eq!(d.likes.as_str(), "1,500");
        assert_eq!(d.added.as_str(), "2026-01-02");
        assert_eq!(d.updated.as_str(), "6 days ago");
        assert_eq!(d.description.as_str(), "Hi there");
        // Only https links are offered.
        assert!(!d.has_link);
    }

    #[test]
    fn heroes_are_distinct_and_sorted_and_labelled() {
        let m = |h: Option<&str>| ModDto {
            hero: h.map(str::to_owned),
            ..ModDto::default()
        };
        let mods = [
            m(Some("Viscous")),
            m(Some("haze")),
            m(Some("Haze")),
            m(None),
            m(Some(" ")),
        ];
        assert_eq!(heroes(&mods), ["haze", "Viscous"]);
        assert_eq!(heroes_label(&[]), "All heroes");
        assert_eq!(heroes_label(&["Haze".into()]), "Haze");
        assert_eq!(heroes_label(&["Haze".into(), "Seven".into()]), "2 heroes");
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
