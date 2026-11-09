//! Crosshair screen controller: edits a `CrosshairConfig`, mirrors it into
//! `CrosshairState` for the live preview, and writes it to `autoexec.cfg`.
//!
//! Edits are pure and run on the UI thread. Reading and writing
//! `autoexec.cfg` run on worker threads that post back with
//! `upgrade_in_event_loop`, like the other screens. Showing the screen
//! reloads the managed block unless there are unapplied edits. Apply is
//! refused while another apply runs, and while Deadlock runs (checked on the
//! worker), as the old app refused it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use shiv_core::crosshair::{self, CrosshairConfig, CrosshairError, Rgb};
use slint::{Color, ComponentHandle, ModelRc, VecModel, Weak};

use crate::browse::game_error_text;
use crate::installed::{Locate, Running};
use crate::{App, CrosshairField, CrosshairState, CrosshairValues};

/// Where the game is and whether it runs; injectable so tests never touch the
/// real install or depend on a running game.
#[derive(Clone)]
pub(crate) struct Deps {
    pub locate: Locate,
    pub running: Running,
}

impl Deps {
    fn system() -> Self {
        Deps {
            locate: Arc::new(crate::locate_game),
            running: Arc::new(shiv_core::game::is_running),
        }
    }
}

const GAME_RUNNING_TEXT: &str = "Deadlock is running. Close it, then apply again.";

/// Quick colour choices: white, red, green, cyan, yellow, magenta.
pub(crate) const PRESETS: [Rgb; 6] = [
    Rgb {
        r: 255,
        g: 255,
        b: 255,
    },
    Rgb { r: 255, g: 0, b: 0 },
    Rgb { r: 0, g: 255, b: 0 },
    Rgb {
        r: 0,
        g: 255,
        b: 255,
    },
    Rgb {
        r: 255,
        g: 255,
        b: 0,
    },
    Rgb {
        r: 255,
        g: 0,
        b: 255,
    },
];

thread_local! {
    static CROSSHAIR: RefCell<Option<Rc<Crosshair>>> = const { RefCell::new(None) };
}

fn with_crosshair(f: impl FnOnce(&Rc<Crosshair>)) {
    if let Some(c) = CROSSHAIR.with(|c| c.borrow().clone()) {
        f(&c);
    }
}

// ---------------------------------------------------------------------------
// Pure edits and view-models (unit-tested)

fn snap(v: f32, step: f32, (min, max): (f32, f32)) -> f32 {
    let snapped = ((v / step).round() * step).clamp(min, max);
    // Drop float noise (0.30000001) so the commands read cleanly.
    let tidy = (snapped * 10.0).round() / 10.0;
    if tidy == 0.0 { 0.0 } else { tidy }
}

fn channel(v: f32) -> u8 {
    // Clamped to 0..=255 first, so the cast cannot truncate.
    v.round().clamp(0.0, 255.0) as u8
}

/// `c` with `field` set to `value`, snapped to the field's step and range.
pub(crate) fn edited(mut c: CrosshairConfig, field: CrosshairField, value: f32) -> CrosshairConfig {
    let value = if value.is_finite() { value } else { 0.0 };
    match field {
        CrosshairField::Red => c.color.r = channel(value),
        CrosshairField::Green => c.color.g = channel(value),
        CrosshairField::Blue => c.color.b = channel(value),
        CrosshairField::PipOpacity => c.pip_opacity = snap(value, 0.1, crosshair::OPACITY_RANGE),
        CrosshairField::PipWidth => c.width = snap(value, 0.1, crosshair::SIZE_RANGE),
        CrosshairField::PipHeight => c.height = snap(value, 0.1, crosshair::SIZE_RANGE),
        CrosshairField::PipGap => c.gap = snap(value, 1.0, crosshair::GAP_RANGE),
        CrosshairField::DotOpacity => c.dot_opacity = snap(value, 0.1, crosshair::OPACITY_RANGE),
        CrosshairField::DotOutlineOpacity => {
            c.dot_outline_opacity = snap(value, 0.1, crosshair::OPACITY_RANGE);
        }
    }
    c
}

fn color(rgb: Rgb) -> Color {
    Color::from_rgb_u8(rgb.r, rgb.g, rgb.b)
}

pub(crate) fn view(c: &CrosshairConfig) -> CrosshairValues {
    CrosshairValues {
        red: c.color.r.into(),
        green: c.color.g.into(),
        blue: c.color.b.into(),
        color: color(c.color),
        hex: format!("#{:02X}{:02X}{:02X}", c.color.r, c.color.g, c.color.b).into(),
        pip_border: c.pip_border,
        pip_gap_static: c.pip_gap_static,
        pip_opacity: c.pip_opacity,
        pip_width: c.width,
        pip_height: c.height,
        pip_gap: c.gap,
        dot_opacity: c.dot_opacity,
        dot_outline_opacity: c.dot_outline_opacity,
        gap_px: c.gap_px(),
    }
}

/// Where the shown crosshair stands relative to `autoexec.cfg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Nothing read yet, or the file could not be read.
    Unknown,
    /// `autoexec.cfg` has no crosshair block: the defaults are shown.
    NoneSaved,
    /// The shown crosshair is the one in `autoexec.cfg`.
    Saved,
}

pub(crate) fn status_text(origin: Origin, dirty: bool) -> &'static str {
    match (dirty, origin) {
        (true, Origin::NoneSaved | Origin::Unknown) => "Not applied yet",
        (true, Origin::Saved) => "Changes not applied yet",
        (false, Origin::Saved) => "Same as the crosshair in autoexec.cfg",
        (false, Origin::NoneSaved) => {
            "No crosshair in autoexec.cfg yet; showing the game's default"
        }
        (false, Origin::Unknown) => "Showing the game's default",
    }
}

pub(crate) fn error_text(e: &CrosshairError) -> String {
    let text = e.to_string();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => text,
    }
}

// ---------------------------------------------------------------------------
// Worker side

type Loaded = Result<(Option<CrosshairConfig>, bool), String>;

fn load(deps: &Deps) -> Loaded {
    let paths = (deps.locate)().map_err(|e| game_error_text(&e))?;
    let running = (deps.running)();
    crosshair::load(&paths)
        .map(|c| (c, running))
        .map_err(|e| error_text(&e))
}

/// `Err((text, game_running))`.
fn write(deps: &Deps, c: &CrosshairConfig) -> Result<(), (String, bool)> {
    let paths = (deps.locate)().map_err(|e| (game_error_text(&e), false))?;
    if (deps.running)() {
        return Err((GAME_RUNNING_TEXT.into(), true));
    }
    crosshair::apply(&paths, c).map_err(|e| (error_text(&e), false))
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .is_ok()
}

// ---------------------------------------------------------------------------
// Controller

struct Crosshair {
    weak: Weak<App>,
    deps: Deps,
    config: Cell<CrosshairConfig>,
    origin: Cell<Origin>,
    /// Edited since the last load or apply.
    dirty: Cell<bool>,
    /// Bumped by every edit, so a load that finishes after one is ignored.
    edits: Cell<u64>,
}

/// Wire the Crosshair screen to `app` using the real game location.
pub fn attach(app: &App) {
    attach_with(app, Deps::system());
}

pub(crate) fn attach_with(app: &App, deps: Deps) {
    let controller = Rc::new(Crosshair {
        weak: app.as_weak(),
        deps,
        config: Cell::new(CrosshairConfig::default()),
        origin: Cell::new(Origin::Unknown),
        dirty: Cell::new(false),
        edits: Cell::new(0),
    });
    CROSSHAIR.with(|c| *c.borrow_mut() = Some(controller.clone()));
    let state = app.global::<CrosshairState>();
    state.set_presets(ModelRc::new(VecModel::from(
        PRESETS.iter().map(|&p| color(p)).collect::<Vec<_>>(),
    )));
    state.on_shown(|| with_crosshair(|c| c.load()));
    state.on_edit(|field, value| with_crosshair(|c| c.set(edited(c.config.get(), field, value))));
    state.on_toggle_border(|| {
        with_crosshair(|c| {
            let mut next = c.config.get();
            next.pip_border = !next.pip_border;
            c.set(next);
        });
    });
    state.on_toggle_static(|| {
        with_crosshair(|c| {
            let mut next = c.config.get();
            next.pip_gap_static = !next.pip_gap_static;
            c.set(next);
        });
    });
    state.on_pick_preset(|i| {
        with_crosshair(|c| {
            if let Some(&rgb) = usize::try_from(i).ok().and_then(|i| PRESETS.get(i)) {
                let mut next = c.config.get();
                next.color = rgb;
                c.set(next);
            }
        });
    });
    state.on_reset(|| with_crosshair(|c| c.set(CrosshairConfig::default())));
    state.on_apply(|| with_crosshair(|c| c.apply()));
    state.on_dismiss_result(|| with_crosshair(|c| c.show_result("", "", false)));
    controller.publish();
}

impl Crosshair {
    fn with_state(&self, f: impl FnOnce(&CrosshairState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<CrosshairState>());
        }
    }

    fn busy(&self) -> bool {
        self.weak
            .upgrade()
            .is_none_or(|a| a.global::<CrosshairState>().get_busy())
    }

    /// Push the config, its commands and the status line to the screen.
    fn publish(&self) {
        let c = self.config.get();
        let status = status_text(self.origin.get(), self.dirty.get());
        self.with_state(|s| {
            s.set_values(view(&c));
            s.set_commands(crosshair::to_commands(&c).into());
            s.set_status(status.into());
        });
    }

    /// An edit from the screen. Ignored while an apply writes the file.
    fn set(&self, next: CrosshairConfig) {
        if self.busy() {
            return;
        }
        self.edits.set(self.edits.get() + 1);
        if next != self.config.get() {
            self.config.set(next);
            self.dirty.set(true);
        }
        self.publish();
    }

    /// Read the managed block, unless there are unapplied edits to keep.
    fn load(&self) {
        let skip = self.weak.upgrade().is_none_or(|a| {
            let s = a.global::<CrosshairState>();
            s.get_busy() || s.get_loading()
        });
        if skip {
            return;
        }
        self.with_state(|s| s.set_loading(true));
        let edits = self.edits.get();
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-crosshair-load", move || {
            let result = load(&deps);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_crosshair(|c| c.loaded(edits, result));
            });
        });
        if !started {
            self.loaded(edits, Err("Could not start a background thread.".into()));
        }
    }

    fn loaded(&self, edits: u64, result: Loaded) {
        self.with_state(|s| s.set_loading(false));
        match result {
            Ok((saved, running)) => {
                self.with_state(|s| s.set_game_running(running));
                // Keep what the user is editing: only an untouched screen
                // takes the file's crosshair.
                if !self.dirty.get() && edits == self.edits.get() {
                    match saved {
                        Some(c) => {
                            self.config.set(c);
                            self.origin.set(Origin::Saved);
                        }
                        None => {
                            self.config.set(CrosshairConfig::default());
                            self.origin.set(Origin::NoneSaved);
                        }
                    }
                }
            }
            Err(text) => {
                self.origin.set(Origin::Unknown);
                self.show_result("Couldn't read autoexec.cfg", &text, true);
            }
        }
        self.publish();
    }

    fn apply(&self) {
        let loading = self
            .weak
            .upgrade()
            .is_none_or(|a| a.global::<CrosshairState>().get_loading());
        if self.busy() || loading {
            return;
        }
        let c = self.config.get();
        self.with_state(|s| s.set_busy(true));
        self.show_result("", "", false);
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-crosshair-apply", move || {
            let result = write(&deps, &c);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_crosshair(|x| x.applied(c, result));
            });
        });
        if !started {
            self.applied(
                c,
                Err(("Could not start a background thread.".into(), false)),
            );
        }
    }

    fn applied(&self, written: CrosshairConfig, result: Result<(), (String, bool)>) {
        self.with_state(|s| s.set_busy(false));
        match result {
            Ok(()) => {
                self.origin.set(Origin::Saved);
                self.dirty.set(self.config.get() != written);
                self.with_state(|s| s.set_game_running(false));
                self.show_result(
                    "Crosshair applied",
                    "Written to autoexec.cfg. Deadlock uses it the next time it starts.",
                    false,
                );
            }
            Err((text, running)) => {
                if running {
                    self.with_state(|s| s.set_game_running(true));
                }
                self.show_result("Couldn't apply the crosshair", &text, true);
            }
        }
        self.publish();
    }

    fn show_result(&self, title: &str, detail: &str, error: bool) {
        self.with_state(|s| {
            s.set_result_title(title.into());
            s.set_result_detail(detail.into());
            s.set_result_error(error);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_snap_to_the_schema_steps_and_ranges() {
        let c = CrosshairConfig::default();
        assert_eq!(edited(c, CrosshairField::PipGap, 3.4).gap, 3.0);
        assert_eq!(edited(c, CrosshairField::PipGap, -99.0).gap, -20.0);
        assert_eq!(edited(c, CrosshairField::PipWidth, 2.3000002).width, 2.3);
        assert_eq!(edited(c, CrosshairField::PipHeight, 400.0).height, 100.0);
        assert_eq!(edited(c, CrosshairField::PipOpacity, 0.34).pip_opacity, 0.3);
        assert_eq!(edited(c, CrosshairField::DotOpacity, -1.0).dot_opacity, 0.0);
        assert_eq!(
            edited(c, CrosshairField::DotOutlineOpacity, 0.75).dot_outline_opacity,
            0.8
        );
        assert_eq!(edited(c, CrosshairField::Red, 12.6).color.r, 13);
        assert_eq!(edited(c, CrosshairField::Green, 999.0).color.g, 255);
        assert_eq!(edited(c, CrosshairField::Blue, f32::NAN).color.b, 0);
        // Snapped values print cleanly.
        let w = edited(c, CrosshairField::PipWidth, 0.1 + 0.2);
        assert!(crosshair::to_commands(&w).contains("citadel_crosshair_pip_width 0.3;"));
        let g = edited(c, CrosshairField::PipGap, -0.2);
        assert!(crosshair::to_commands(&g).contains("citadel_crosshair_pip_gap 0;"));
    }

    #[test]
    fn view_carries_colour_and_gap() {
        let c = CrosshairConfig {
            color: Rgb {
                r: 0,
                g: 255,
                b: 128,
            },
            gap: 2.0,
            ..CrosshairConfig::default()
        };
        let v = view(&c);
        assert_eq!(v.hex.as_str(), "#00FF80");
        assert_eq!(v.color, Color::from_rgb_u8(0, 255, 128));
        assert_eq!((v.red, v.green, v.blue), (0, 255, 128));
        assert_eq!(v.gap_px, 15.0);
    }

    #[test]
    fn status_reads_plainly() {
        assert_eq!(
            status_text(Origin::Saved, false),
            "Same as the crosshair in autoexec.cfg"
        );
        assert_eq!(status_text(Origin::Saved, true), "Changes not applied yet");
        assert_eq!(status_text(Origin::NoneSaved, true), "Not applied yet");
        assert!(status_text(Origin::NoneSaved, false).starts_with("No crosshair"));
        let e = CrosshairError::NotText("/g/autoexec.cfg".into());
        assert_eq!(
            error_text(&e),
            "/g/autoexec.cfg is not plain text, so Shiv left it unchanged."
        );
    }
}
