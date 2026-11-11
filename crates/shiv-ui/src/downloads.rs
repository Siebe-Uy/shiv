//! Downloads screen: a passive view over the installs Browse runs.
//!
//! Browse owns the install machinery (workers, cancellation, the in-flight
//! guards); it reports four events here: started, progress, installing and
//! finished. This module keeps the list (newest first, a bounded history for
//! this session) and turns Cancel and Retry back into Browse calls.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::browse::fmt_mb;
use crate::{App, DownloadPhase, DownloadsState, TransferRow};

/// Finished rows kept; older ones drop off.
const HISTORY: usize = 30;
/// Speed is averaged over at least this long, so it does not flicker.
const SPEED_WINDOW: Duration = Duration::from_millis(750);

struct Item {
    id: String,
    name: String,
    phase: DownloadPhase,
    progress: f32,
    done: u64,
    total: Option<u64>,
    speed: f64,
    sample: (Instant, u64),
    detail: String,
}

struct Downloads {
    weak: Weak<App>,
    rows: Rc<VecModel<TransferRow>>,
    items: RefCell<Vec<Item>>,
}

thread_local! {
    static DOWNLOADS: RefCell<Option<Rc<Downloads>>> = const { RefCell::new(None) };
}

fn with_downloads(f: impl FnOnce(&Rc<Downloads>)) {
    if let Some(d) = DOWNLOADS.with(|c| c.borrow().clone()) {
        f(&d);
    }
}

pub fn attach(app: &App) {
    let rows = Rc::new(VecModel::default());
    let controller = Rc::new(Downloads {
        weak: app.as_weak(),
        rows: rows.clone(),
        items: RefCell::new(Vec::new()),
    });
    let state = app.global::<DownloadsState>();
    state.set_rows(ModelRc::from(rows));
    DOWNLOADS.with(|c| *c.borrow_mut() = Some(controller));
    state.on_cancel(|id| crate::browse::cancel_install(id.as_str()));
    state.on_retry(|id| crate::browse::retry_install(id.as_str()));
    state.on_clear_finished(|| with_downloads(|d| d.clear_finished()));
}

/// An install's download began.
pub(crate) fn started(id: &str, name: &str) {
    with_downloads(|d| d.started(id, name));
}

/// Bytes received so far (`total` when the size is known).
pub(crate) fn progress(id: &str, done: u64, total: Option<u64>) {
    with_downloads(|d| d.progress(id, done, total, Instant::now()));
}

/// An install that never reached the download (no file list, no files).
pub(crate) fn failed_to_start(id: &str, name: &str, reason: &str) {
    started(id, name);
    finished(id, Outcome::Failed(reason.to_owned()));
}

pub(crate) fn installing(id: &str) {
    with_downloads(|d| d.set_phase(id, DownloadPhase::Installing, "Installing".into()));
}

pub(crate) fn finished(id: &str, outcome: Outcome) {
    with_downloads(|d| match outcome {
        Outcome::Done => d.set_phase(id, DownloadPhase::Done, "Installed".into()),
        Outcome::Cancelled => d.set_phase(id, DownloadPhase::Cancelled, "Cancelled".into()),
        Outcome::Failed(text) => d.set_phase(id, DownloadPhase::Failed, text),
    });
}

/// A running transfer of `id`: its progress (negative when the size is
/// unknown) and whether it has reached the install step. `None` otherwise.
pub(crate) fn transfer(id: &str) -> Option<(f32, bool)> {
    let mut out = None;
    with_downloads(|d| {
        out = d
            .items
            .borrow()
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| match i.phase {
                DownloadPhase::Downloading => Some((i.progress, false)),
                DownloadPhase::Installing => Some((1.0, true)),
                _ => None,
            });
    });
    out
}

pub(crate) enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

fn speed_text(bytes_per_sec: f64) -> String {
    format!("{}/s", fmt_mb(bytes_per_sec as u64))
}

/// "12.3 MB of 52.0 MB \u{b7} 4.2 MB/s", or just what is known.
fn progress_text(done: u64, total: Option<u64>, speed: f64) -> String {
    let mut text = match total {
        Some(t) => format!("{} of {}", fmt_mb(done), fmt_mb(t)),
        None => fmt_mb(done),
    };
    if speed > 0.0 {
        text.push_str(&format!(" \u{b7} {}", speed_text(speed)));
    }
    text
}

impl Downloads {
    fn started(&self, id: &str, name: &str) {
        {
            let mut items = self.items.borrow_mut();
            items.retain(|i| i.id != id);
            items.insert(
                0,
                Item {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    phase: DownloadPhase::Downloading,
                    progress: -1.0,
                    done: 0,
                    total: None,
                    speed: 0.0,
                    sample: (Instant::now(), 0),
                    detail: "Starting".into(),
                },
            );
        }
        self.sync();
        crate::installed::transfer_changed(id);
    }

    fn progress(&self, id: &str, done: u64, total: Option<u64>, now: Instant) {
        {
            let mut items = self.items.borrow_mut();
            let Some(item) = items
                .iter_mut()
                .find(|i| i.id == id && i.phase == DownloadPhase::Downloading)
            else {
                return;
            };
            let elapsed = now.saturating_duration_since(item.sample.0);
            if elapsed >= SPEED_WINDOW {
                let instant = done.saturating_sub(item.sample.1) as f64 / elapsed.as_secs_f64();
                // Smooth: half the previous value, half the new window.
                item.speed = if item.speed > 0.0 {
                    (item.speed + instant) / 2.0
                } else {
                    instant
                };
                item.sample = (now, done);
            }
            item.done = done;
            item.total = total.filter(|t| *t > 0);
            item.progress = item
                .total
                .map_or(-1.0, |t| (done as f64 / t as f64).clamp(0.0, 1.0) as f32);
            item.detail = progress_text(done, item.total, item.speed);
        }
        self.sync();
        crate::installed::transfer_changed(id);
    }

    fn set_phase(&self, id: &str, phase: DownloadPhase, detail: String) {
        {
            let mut items = self.items.borrow_mut();
            let Some(item) = items.iter_mut().find(|i| i.id == id) else {
                return;
            };
            // A late event for a row that already ended (a cancel races the
            // worker's own finish) must not overwrite the outcome.
            let ended = matches!(
                item.phase,
                DownloadPhase::Done | DownloadPhase::Failed | DownloadPhase::Cancelled
            );
            if ended && phase != DownloadPhase::Done {
                return;
            }
            item.phase = phase;
            item.detail = detail;
            if phase == DownloadPhase::Done {
                item.progress = 1.0;
            }
            // Keep the history bounded: only finished rows count.
            let mut finished = 0;
            items.retain(|i| {
                let ended = matches!(
                    i.phase,
                    DownloadPhase::Done | DownloadPhase::Failed | DownloadPhase::Cancelled
                );
                if ended {
                    finished += 1;
                }
                !ended || finished <= HISTORY
            });
        }
        self.sync();
        crate::installed::transfer_changed(id);
    }

    fn clear_finished(&self) {
        self.items.borrow_mut().retain(|i| {
            matches!(
                i.phase,
                DownloadPhase::Downloading | DownloadPhase::Installing
            )
        });
        self.sync();
    }

    fn sync(&self) {
        let items = self.items.borrow();
        self.rows.set_vec(
            items
                .iter()
                .map(|i| TransferRow {
                    id: i.id.as_str().into(),
                    name: i.name.as_str().into(),
                    phase: i.phase,
                    progress: i.progress,
                    detail: i.detail.as_str().into(),
                })
                .collect::<Vec<_>>(),
        );
        let active = items
            .iter()
            .filter(|i| {
                matches!(
                    i.phase,
                    DownloadPhase::Downloading | DownloadPhase::Installing
                )
            })
            .count();
        drop(items);
        if let Some(app) = self.weak.upgrade() {
            app.global::<DownloadsState>()
                .set_active_count(active as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_text_shows_what_is_known() {
        assert_eq!(progress_text(1_500_000, None, 0.0), "1.5 MB");
        assert_eq!(
            progress_text(12_300_000, Some(52_000_000), 4_200_000.0),
            "12.3 MB of 52.0 MB \u{b7} 4.2 MB/s"
        );
    }
}
