slint::include_modules!();

use slint::{Image, Model, ModelNotify, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer, SharedString};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

const ROWS: usize = 1000;
const W: u32 = 240;
const H: u32 = 135;
const SCROLL_PX_PER_S: f32 = 3000.0;
const SCROLL_SECS: f64 = 10.0;

fn make_image(i: usize) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(W, H);
    let seed = (i as u32).wrapping_mul(2654435761);
    let (r0, g0, b0) = ((seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8);
    for (n, p) in buf.make_mut_slice().iter_mut().enumerate() {
        let x = (n as u32 % W) as u8;
        let y = (n as u32 / W) as u8;
        *p = Rgba8Pixel { r: r0.wrapping_add(x), g: g0.wrapping_add(y), b: b0 ^ x.wrapping_mul(y), a: 255 };
    }
    Image::from_rgba8(buf)
}

/// Rows are generated on demand; only a small window of images is kept resident.
struct LazyRows {
    notify: ModelNotify,
    cache: RefCell<VecDeque<(usize, Image)>>,
}

impl Model for LazyRows {
    type Data = Row;
    fn row_count(&self) -> usize { ROWS }
    fn row_data(&self, i: usize) -> Option<Row> {
        if i >= ROWS { return None; }
        let mut cache = self.cache.borrow_mut();
        let img = match cache.iter().find(|(k, _)| *k == i) {
            Some((_, img)) => img.clone(),
            None => {
                let img = make_image(i);
                cache.push_back((i, img.clone()));
                if cache.len() > 48 { cache.pop_front(); }
                img
            }
        };
        Some(Row {
            title: SharedString::from(format!("Mod number {i}")),
            subtitle: SharedString::from(format!("Author {} - {} downloads", i % 97, i * 31 % 10007)),
            img,
        })
    }
    fn model_tracker(&self) -> &dyn ModelTracker { &self.notify }
}

fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines().find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0) / 1024.0
}

thread_local! { static LAST: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
    static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; static T0: std::time::Instant = std::time::Instant::now(); }

fn cpu_secs() -> f64 {
    // utime + stime (clock ticks, 100/s) from /proc/self/stat, fields 14 and 15
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = s.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    (f.get(11).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) + f.get(12).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)) / 100.0
}

fn main() {
    T0.with(|_| ());
    let ui = App::new().unwrap();
    let model = Rc::new(LazyRows { notify: ModelNotify::default(), cache: RefCell::new(VecDeque::new()) });
    ui.set_rows(ModelRc::from(model));

    // Phases: warm-up 1.5 s idle-ish (frames still tick), then scripted scroll for 10 s.
    let state = Rc::new(RefCell::new((None::<Duration>, None::<Duration>, Vec::<f64>::new(), 0.0f64, false)));
    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_frame(move |tick| {
        // one sample per animation tick (= per rendered frame); ignore repeat notifications
        if LAST.with(|l| l.replace(tick)) == tick { return; }
        let t = T0.with(|t| t.elapsed());
        let mut s = st.borrow_mut();
        let start = *s.0.get_or_insert(t);
        let el = (t - start).as_secs_f64();
        if el < 1.5 { return; }
        if !s.4 { s.4 = true; println!("rss_after_load_mb={:.1} cpu_secs_at_load={:.2}", rss_mb(), cpu_secs()); }
        let scroll_t = el - 1.5;
        if let Some(prev) = s.1 { s.2.push((t - prev).as_secs_f64() * 1000.0); }
        s.1 = Some(t);
        if scroll_t >= SCROLL_SECS {
            if DONE.with(|d| d.replace(true)) { return; }
            let mut v = s.2.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let pct = |p: f64| v[((v.len() as f64 - 1.0) * p) as usize];
            let under = v.iter().filter(|x| **x < 16.7).count() as f64 / v.len() as f64;
            println!("frames={} median_ms={:.2} p95_ms={:.2} p99_ms={:.2} max_ms={:.2} frac_under_16.7={:.3}",
                v.len(), pct(0.5), pct(0.95), pct(0.99), v[v.len() - 1], under);
            println!("rss_after_scroll_mb={:.1} cpu_secs_total={:.2}", rss_mb(), cpu_secs());
            let _ = slint::quit_event_loop();
            return;
        }
        s.3 = scroll_t * SCROLL_PX_PER_S as f64;
        if let Some(ui) = weak.upgrade() { ui.set_scroll_y(-(s.3 as f32)); }
    });
    ui.run().unwrap();
}
