//! Thumbnail cache: fetches, decodes to display size and keeps an LRU of
//! decoded RGBA pixels under a byte budget. Window-free so it is unit-testable;
//! conversion to `slint::Image` happens at the UI edge.

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
    fetch: Box<Fetch>,
    inner: Mutex<Inner>,
}

impl Cache {
    pub fn new(max_bytes: usize) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Self::with_fetcher(max_bytes, move |url| http_get(&agent, url))
    }

    pub fn with_fetcher(
        max_bytes: usize,
        fetch: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            max_bytes,
            fetch: Box::new(fetch),
            inner: Mutex::new(Inner::default()),
        }
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
        let decoded = (self.fetch)(url).and_then(|bytes| decode(&bytes));
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

/// Decode and cover-crop to exactly `DISPLAY_W` x `DISPLAY_H`.
fn decode(bytes: &[u8]) -> Option<Image> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let rgba = img
        .resize_to_fill(DISPLAY_W, DISPLAY_H, FilterType::Triangle)
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
    fn cache_decodes_to_display_size() {
        // Wider than 16:9 so the cover crop is exercised.
        let cache = Cache::with_fetcher(10 * THUMB_BYTES, |_| Some(png(1000, 400, [200, 100, 50])));
        let img = cache.get_or_fetch("x").expect("decoded image");
        assert_eq!((img.width(), img.height()), (DISPLAY_W, DISPLAY_H));
        assert_eq!(img.rgba().len(), THUMB_BYTES);
        assert_eq!(&img.rgba()[..4], &[200, 100, 50, 255]);
    }
}
