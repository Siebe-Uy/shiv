//! Image cache: fetches, decodes to display size and keeps an LRU of decoded
//! RGBA pixels under a byte budget. One instance holds list thumbnails (cover
//! crop to a fixed size), another the detail page's gallery (fit inside a
//! box, aspect kept). Window-free so it is unit-testable; conversion to
//! `slint::Image` happens at the UI edge.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use image::imageops::FilterType;

/// Display size of a list thumbnail.
pub const DISPLAY_W: u32 = 240;
pub const DISPLAY_H: u32 = 135;

/// Largest encoded image accepted from the network.
const MAX_DOWNLOAD: u64 = 10 * 1024 * 1024;
/// Remembered failed URLs before the list is reset (keeps it bounded).
const MAX_FAILED: usize = 4096;
/// Decode limits per image. With several workers decoding at once these bound
/// the transient memory spike (thumbnail sources are ~530 px wide).
const MAX_DIMENSION: u32 = 4096;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

/// How decoded images are sized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Scaled and cropped to exactly this size (list thumbnails).
    Fill(u32, u32),
    /// Scaled down to fit inside this box, aspect kept; never scaled up.
    Fit(u32, u32),
}

/// A decoded thumbnail: tightly packed RGBA8 pixels.
#[derive(Debug, Clone)]
pub struct Image {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl Image {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

type Fetch = dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync;

struct Entry {
    image: Image,
    last_used: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    used: usize,
    clock: u64,
    failed: HashSet<String>,
}

impl Inner {
    fn touch(&mut self, url: &str) -> Option<Image> {
        self.clock += 1;
        let now = self.clock;
        let entry = self.entries.get_mut(url)?;
        entry.last_used = now;
        Some(entry.image.clone())
    }

    fn insert(&mut self, url: &str, image: Image, max_bytes: usize) {
        let size = image.rgba.len();
        if size > max_bytes {
            return;
        }
        self.clock += 1;
        let entry = Entry {
            image,
            last_used: self.clock,
        };
        if let Some(old) = self.entries.insert(url.to_owned(), entry) {
            self.used -= old.image.rgba.len();
        }
        self.used += size;
        while self.used > max_bytes {
            let Some(lru) = self
                .entries
                .iter()
                .filter(|(k, _)| k.as_str() != url)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = self.entries.remove(&lru) {
                self.used -= e.image.rgba.len();
            }
        }
    }
}

/// Thread-safe; `get_or_fetch` blocks on the network and belongs on a worker
/// thread, `get` only reads the cache and is safe on the UI thread.
pub struct Cache {
    max_bytes: usize,
    shape: Shape,
    fetch: Box<Fetch>,
    inner: Mutex<Inner>,
}

impl Cache {
    /// List thumbnails, cover-cropped to `DISPLAY_W` x `DISPLAY_H`.
    pub fn new(max_bytes: usize) -> Self {
        Self::fetching(max_bytes, Shape::Fill(DISPLAY_W, DISPLAY_H))
    }

    /// Fetching over HTTP, decoding to `shape`.
    pub fn fetching(max_bytes: usize, shape: Shape) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Self::with_fetcher(max_bytes, move |url| http_get(&agent, url)).shaped(shape)
    }

    pub fn with_fetcher(
        max_bytes: usize,
        fetch: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            max_bytes,
            shape: Shape::Fill(DISPLAY_W, DISPLAY_H),
            fetch: Box::new(fetch),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The same cache decoding to `shape` instead.
    pub fn shaped(mut self, shape: Shape) -> Self {
        self.shape = shape;
        self
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Cached image only; never fetches. Counts as a use for LRU.
    pub fn get(&self, url: &str) -> Option<Image> {
        self.lock().touch(url)
    }

    /// True when fetching or decoding `url` failed before.
    pub fn has_failed(&self, url: &str) -> bool {
        self.lock().failed.contains(url)
    }

    /// Forget past failures so they are fetched again (used on Retry).
    pub fn clear_failed(&self) {
        self.lock().failed.clear();
    }

    /// Drop one decoded image (a result nobody wants any more).
    pub fn remove(&self, url: &str) {
        let mut inner = self.lock();
        if let Some(e) = inner.entries.remove(url) {
            inner.used -= e.image.rgba.len();
        }
    }

    /// Drop every decoded image and remembered failure.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.entries = HashMap::new();
        inner.used = 0;
        inner.failed = HashSet::new();
    }

    /// Bytes of decoded pixels held, and how many images.
    #[cfg(test)]
    pub fn usage(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.used, inner.entries.len())
    }

    /// Cached image, or fetch and decode it (blocking). The lock is not held
    /// while fetching, so several workers can fetch at once.
    pub fn get_or_fetch(&self, url: &str) -> Option<Image> {
        {
            let mut inner = self.lock();
            if let Some(img) = inner.touch(url) {
                return Some(img);
            }
            if inner.failed.contains(url) {
                return None;
            }
        }
        // A panicking decoder (a malformed image) counts as a failed image.
        let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.fetch)(url).and_then(|bytes| decode(&bytes, self.shape))
        }))
        .ok()
        .flatten();
        let mut inner = self.lock();
        match decoded {
            Some(img) => {
                inner.insert(url, img.clone(), self.max_bytes);
                Some(img)
            }
            None => {
                if inner.failed.len() >= MAX_FAILED {
                    inner.failed.clear();
                }
                inner.failed.insert(url.to_owned());
                None
            }
        }
    }
}

fn http_get(agent: &ureq::Agent, url: &str) -> Option<Vec<u8>> {
    let mut resp = agent.get(url).call().ok()?;
    resp.body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .ok()
}

/// Grid the pixels are averaged down to for [`obscured`]: 16x9 cells of 15 px
/// each at display size, so no shape smaller than a sixth of the frame survives.
const OBSCURE_W: u32 = 16;
const OBSCURE_H: u32 = 9;
/// Size the averaged grid is smoothed back up to (half the display size; the
/// UI scales it the rest of the way).
pub const OBSCURED_W: u32 = DISPLAY_W / 2;
pub const OBSCURED_H: u32 = DISPLAY_H / 2;

/// A heavily blurred copy of `img` for content the viewer has not opted into
/// (NSFW). The detail is really gone: the pixels are averaged down to a 16x9
/// grid and smoothed back up, so nothing of the original survives beyond
/// broad colour fields. About 32 KB, a fraction of a millisecond to make.
pub fn obscured(img: &Image) -> Option<Image> {
    obscure(img, (OBSCURE_W, OBSCURE_H), (OBSCURED_W, OBSCURED_H))
}

/// [`obscured`] for an image of any size and aspect (the detail gallery):
/// the longer side is averaged down to 16 cells, the shorter one in
/// proportion, and the result is a quarter of the original size.
pub fn obscured_any(img: &Image) -> Option<Image> {
    let long = img.width.max(img.height).max(1);
    let cells = |side: u32| (side * OBSCURE_W).div_ceil(long).max(1);
    let grid = (cells(img.width), cells(img.height));
    let out = ((img.width / 4).max(1), (img.height / 4).max(1));
    obscure(img, grid, out)
}

fn obscure(img: &Image, grid: (u32, u32), out: (u32, u32)) -> Option<Image> {
    let src = image::RgbaImage::from_raw(img.width, img.height, img.rgba.to_vec())?;
    let small = image::imageops::resize(&src, grid.0, grid.1, FilterType::Triangle);
    let big = image::imageops::resize(&small, out.0, out.1, FilterType::Triangle);
    Some(Image {
        width: out.0,
        height: out.1,
        rgba: Arc::from(big.into_raw()),
    })
}

/// Decode and size to `shape`.
fn decode(bytes: &[u8], shape: Shape) -> Option<Image> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let rgba = match shape {
        Shape::Fill(w, h) => img.resize_to_fill(w, h, FilterType::Triangle),
        Shape::Fit(w, h) if img.width() > w || img.height() > h => {
            img.resize(w, h, FilterType::Triangle)
        }
        Shape::Fit(..) => img,
    }
    .into_rgba8();
    let (width, height) = rgba.dimensions();
    Some(Image {
        width,
        height,
        rgba: Arc::from(rgba.into_raw()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    const THUMB_BYTES: usize = (DISPLAY_W * DISPLAY_H * 4) as usize;

    #[test]
    fn cache_evicts_least_recently_used_over_budget() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let counter = fetches.clone();
        let cache = Cache::with_fetcher(2 * THUMB_BYTES, move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Some(png(64, 36, [10, 20, 30]))
        });

        assert!(cache.get_or_fetch("a").is_some());
        assert!(cache.get_or_fetch("b").is_some());
        // Hit: no fetch, and `a` becomes the most recently used.
        assert!(cache.get_or_fetch("a").is_some());
        assert_eq!(fetches.load(Ordering::SeqCst), 2);

        // Third entry exceeds the budget: `b` (least recently used) goes.
        assert!(cache.get_or_fetch("c").is_some());
        assert_eq!(fetches.load(Ordering::SeqCst), 3);
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_none());
        assert!(cache.get("c").is_some());

        assert!(cache.get_or_fetch("b").is_some());
        assert_eq!(fetches.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_panicking_fetch_or_decode_counts_as_a_failed_image() {
        let cache = Cache::with_fetcher(10 * THUMB_BYTES, |_| panic!("decoder bug"));
        assert!(cache.get_or_fetch("https://x.test/bad.png").is_none());
        assert!(cache.has_failed("https://x.test/bad.png"));
    }

    #[test]
    fn cache_decodes_to_display_size() {
        // Wider than 16:9 so the cover crop is exercised.
        let cache = Cache::with_fetcher(10 * THUMB_BYTES, |_| Some(png(1000, 400, [200, 100, 50])));
        let img = cache.get_or_fetch("x").expect("decoded image");
        assert_eq!((img.width(), img.height()), (DISPLAY_W, DISPLAY_H));
        assert_eq!(img.rgba().len(), THUMB_BYTES);
        assert_eq!(&img.rgba()[..4], &[200, 100, 50, 255]);
    }

    /// Two pictures with the same colours but different fine detail (a
    /// one-pixel checkerboard against its inverse, i.e. "text" vs. its
    /// negative) come out the same: the detail does not survive.
    #[test]
    fn obscured_removes_detail() {
        let make = |invert: bool| {
            let mut rgba = Vec::with_capacity(THUMB_BYTES);
            for y in 0..DISPLAY_H {
                for x in 0..DISPLAY_W {
                    let on = ((x + y) % 2 == 0) != invert;
                    let v = if on { 255 } else { 0 };
                    rgba.extend_from_slice(&[v, v, v, 255]);
                }
            }
            Image {
                width: DISPLAY_W,
                height: DISPLAY_H,
                rgba: Arc::from(rgba),
            }
        };
        let (a, b) = (
            obscured(&make(false)).unwrap(),
            obscured(&make(true)).unwrap(),
        );
        assert_eq!((a.width(), a.height()), (OBSCURED_W, OBSCURED_H));
        assert_eq!(a.rgba().len(), (OBSCURED_W * OBSCURED_H * 4) as usize);
        let max_diff = a
            .rgba()
            .iter()
            .zip(b.rgba())
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap();
        assert!(max_diff <= 4, "detail leaked: {max_diff}");
        // Mid grey everywhere, not a dimmed copy of the pattern.
        assert!(a.rgba().chunks(4).all(|p| (120..=136).contains(&p[0])));
        // A malformed buffer is refused, not a panic.
        let bad = Image {
            width: 10,
            height: 10,
            rgba: Arc::from(vec![0u8; 3]),
        };
        assert!(obscured(&bad).is_none());
    }

    /// The gallery cache: images fit inside the box with their aspect kept
    /// (small ones stay small), the byte budget holds however many are
    /// fetched, and `remove`/`clear` give the memory back.
    #[test]
    fn fit_cache_keeps_aspect_and_stays_within_budget() {
        const BOX: (u32, u32) = (960, 540);
        let full = (BOX.0 * BOX.1 * 4) as usize;
        let cache = Cache::with_fetcher(3 * full, |url| {
            Some(match url {
                "small" => png(300, 200, [1, 2, 3]),
                "tall" => png(500, 1000, [1, 2, 3]),
                _ => png(1920, 1080, [9, 9, 9]),
            })
        })
        .shaped(Shape::Fit(BOX.0, BOX.1));
        let size = |url: &str| {
            let img = cache.get_or_fetch(url).expect("decoded");
            (img.width(), img.height())
        };
        assert_eq!(size("wide-0"), BOX);
        assert_eq!(size("small"), (300, 200));
        assert_eq!(size("tall"), (270, 540));
        for i in 1..20 {
            size(&format!("wide-{i}"));
            let (used, count) = cache.usage();
            assert!(used <= 3 * full, "{used} over budget");
            assert!(count <= 5);
        }
        // The most recent images are the ones kept.
        assert!(cache.get("wide-19").is_some() && cache.get("wide-18").is_some());
        assert!(cache.get("wide-0").is_none());
        cache.remove("wide-19");
        assert!(cache.get("wide-19").is_none());
        assert!(cache.usage().0 <= 2 * full);
        cache.clear();
        assert_eq!(cache.usage(), (0, 0));
    }

    #[test]
    fn obscured_any_keeps_aspect_and_removes_detail() {
        let make = |w: u32, h: u32, invert: bool| {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    let v = if ((x + y) % 2 == 0) != invert { 255 } else { 0 };
                    rgba.extend_from_slice(&[v, v, v, 255]);
                }
            }
            Image {
                width: w,
                height: h,
                rgba: Arc::from(rgba),
            }
        };
        for (w, h) in [(960, 540), (405, 540), (7, 3)] {
            let a = obscured_any(&make(w, h, false)).expect("blurred");
            let b = obscured_any(&make(w, h, true)).expect("blurred");
            assert_eq!((a.width(), a.height()), ((w / 4).max(1), (h / 4).max(1)));
            let max_diff = a
                .rgba()
                .iter()
                .zip(b.rgba())
                .map(|(x, y)| x.abs_diff(*y))
                .max()
                .unwrap();
            assert!(max_diff <= 4, "{w}x{h}: detail leaked ({max_diff})");
        }
    }

    #[test]
    fn cache_rejects_images_over_decode_limits() {
        // Tiny when encoded, but wider than MAX_DIMENSION.
        let cache = Cache::with_fetcher(10 * THUMB_BYTES, |_| {
            Some(png(MAX_DIMENSION + 1, 8, [1, 2, 3]))
        });
        assert!(cache.get_or_fetch("huge").is_none());
        assert!(cache.has_failed("huge"));
        cache.clear_failed();
        assert!(!cache.has_failed("huge"));
        // Garbage bytes are a failure too, not a panic.
        let junk = Cache::with_fetcher(THUMB_BYTES, |_| Some(vec![0xFF, 0xD8, 0xFF, 0x00, 0x01]));
        assert!(junk.get_or_fetch("junk").is_none());
    }
}
