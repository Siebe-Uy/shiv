//! Asks what to do when a mod replaces the same hero's content as mods that
//! are already enabled: keep both, or swap (switch the old ones off; their
//! files stay, so a swap is reversible from Installed). It runs after an
//! install, and before enabling an installed mod from the Installed screen
//! (that offer can also be cancelled, since nothing has changed yet).
//!
//! Offers queue up, so two installs finishing back to back show one dialog
//! after the other. The dialog is a window-wide modal, so it works from any
//! screen.

use std::cell::RefCell;
use std::collections::VecDeque;

use shiv_core::addons;
use shiv_core::game;
use shiv_core::sidecar;
use slint::{ComponentHandle, Weak};

use crate::{App, ConflictState};

struct Offer {
    new_id: String,
    new_name: String,
    /// The installed mod waiting to be enabled; `None` after an install,
    /// where the new mod is already on.
    enable: Option<String>,
    /// `(mod id, name)` of the enabled mods the new one conflicts with.
    with: Vec<(String, String)>,
}

#[derive(Default)]
struct Queue {
    offers: VecDeque<Offer>,
    /// A swap is running on a worker.
    swapping: bool,
}

thread_local! {
    static QUEUE: RefCell<Queue> = RefCell::new(Queue::default());
}

pub(crate) fn attach(app: &App) {
    let state = app.global::<ConflictState>();
    let weak = app.as_weak();
    state.on_keep_both({
        let weak = weak.clone();
        move || keep_both(&weak)
    });
    state.on_swap({
        let weak = weak.clone();
        move || swap(weak.clone())
    });
    state.on_load_first({
        let weak = weak.clone();
        move || load_first(weak.clone())
    });
    state.on_cancel(move || cancel(&weak));
}

/// Queue an offer; shown at once when no other is open.
pub(crate) fn offer(
    weak: &Weak<App>,
    new_id: String,
    new_name: String,
    with: Vec<(String, String)>,
    enable: Option<String>,
) {
    QUEUE.with(|q| {
        q.borrow_mut().offers.push_back(Offer {
            new_id,
            new_name,
            enable,
            with,
        })
    });
    if let Some(app) = weak
        .upgrade()
        .filter(|a| !a.global::<ConflictState>().get_open())
    {
        show_front(&app);
    }
}

fn show_front(app: &App) {
    let state = app.global::<ConflictState>();
    let front = QUEUE.with(|q| {
        q.borrow()
            .offers
            .front()
            .map(|o| (o.new_name.clone(), names(&o.with), o.enable.is_some()))
    });
    match front {
        Some((new_name, old, enabling)) => {
            state.set_enabling(enabling);
            state.set_new_name(new_name.into());
            state.set_old_names(old.into());
            state.set_error_text("".into());
            state.set_open(true);
        }
        None => state.set_open(false),
    }
}

fn names(with: &[(String, String)]) -> String {
    with.iter()
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Leave both mods on: an offer made before enabling now enables the mod;
/// then the next offer is shown.
fn keep_both(weak: &Weak<App>) {
    let Some(app) = weak.upgrade() else { return };
    let dropped = QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        // A running swap owns the front offer.
        if q.swapping {
            return None;
        }
        q.offers.pop_front()
    });
    let Some(offer) = dropped else { return };
    if let Some(id) = offer.enable {
        crate::installed::enable(&id);
    }
    show_front(&app);
}

/// Drop an offer made before enabling, changing nothing.
fn cancel(weak: &Weak<App>) {
    let Some(app) = weak.upgrade() else { return };
    let dropped = QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if q.swapping || q.offers.front().is_none_or(|o| o.enable.is_none()) {
            return false;
        }
        q.offers.pop_front().is_some()
    });
    if dropped {
        show_front(&app);
    }
}

fn swap(weak: Weak<App>) {
    let Some(offer) = claim_front() else { return };
    let ids: Vec<String> = offer.with.iter().map(|(id, _)| id.clone()).collect();
    run(weak, "shiv-conflict-swap", move || {
        disable(&ids, offer.enable.as_deref())
    });
}

/// Leave both mods on and put the new one first in the load order, so it wins.
fn load_first(weak: Weak<App>) {
    let Some(offer) = claim_front() else { return };
    run(weak, "shiv-conflict-first", move || {
        enable_first(&offer.new_id, offer.enable.is_some())
    });
}

/// Take the front offer for a worker; `None` when there is none or another
/// worker already holds it. Marks the swap as running.
fn claim_front() -> Option<Offer> {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if q.swapping {
            return None;
        }
        let front = q.offers.front()?;
        let claimed = Offer {
            new_id: front.new_id.clone(),
            new_name: String::new(),
            enable: front.enable.clone(),
            with: front.with.clone(),
        };
        q.swapping = true;
        Some(claimed)
    })
}

fn run(
    weak: Weak<App>,
    name: &'static str,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) {
    let Some(app) = weak.upgrade() else { return };
    let state = app.global::<ConflictState>();
    state.set_busy(true);
    state.set_error_text("".into());
    let done_weak = weak.clone();
    let started = crate::spawn_worker(name, work, move |result| {
        let result = result.and_then(|r| r);
        let _ = done_weak.upgrade_in_event_loop(move |app| swapped(&app, result));
    });
    if !started {
        swapped(&app, Err("Could not start a background thread.".into()));
    }
}

fn swapped(app: &App, result: Result<(), String>) {
    let state = app.global::<ConflictState>();
    state.set_busy(false);
    QUEUE.with(|q| q.borrow_mut().swapping = false);
    match result {
        Ok(()) => {
            crate::launch::refresh();
            crate::installed::reload();
            QUEUE.with(|q| q.borrow_mut().offers.pop_front());
            show_front(app);
        }
        // Stay open so the user can retry or keep both.
        Err(text) => state.set_error_text(text.into()),
    }
}

fn disable(ids: &[String], enable: Option<&str>) -> Result<(), String> {
    if game::is_running() {
        return Err("Deadlock is running. Close it to swap mods.".into());
    }
    let paths = crate::locate_game().map_err(|e| crate::browse::game_error_text(&e))?;
    let mods = sidecar::group(&paths.addons);
    let set = |id: &str, on: bool| -> Result<(), String> {
        let found = mods
            .iter()
            .filter(|m| m.sidecar.as_ref().is_some_and(|s| s.mod_id == id));
        for m in found {
            for vpk in &m.vpks {
                addons::set_enabled(vpk, on).map_err(|e| {
                    format!(
                        "Could not switch a mod {}. {e}",
                        if on { "on" } else { "off" }
                    )
                })?;
            }
        }
        Ok(())
    };
    // Old ones off first, so the two never fight over the same slot.
    for id in ids {
        set(id, false)?;
    }
    if let Some(id) = enable {
        set(id, true)?;
    }
    Ok(())
}

/// Switch the mod `id` on when `enable`, then move it to the first place.
fn enable_first(id: &str, enable: bool) -> Result<(), String> {
    if game::is_running() {
        return Err("Deadlock is running. Close it to change mods.".into());
    }
    let paths = crate::locate_game().map_err(|e| crate::browse::game_error_text(&e))?;
    let mods = sidecar::group(&paths.addons);
    let is_it = |m: &sidecar::InstalledMod| m.sidecar.as_ref().is_some_and(|s| s.mod_id == id);
    if enable {
        for vpk in mods.iter().filter(|m| is_it(m)).flat_map(|m| &m.vpks) {
            addons::set_enabled(vpk, true)
                .map_err(|e| format!("Could not switch the mod on. {e}"))?;
        }
    }
    // Enabling renames files in place, so list again before moving.
    let mods = sidecar::group(&paths.addons);
    let index = mods
        .iter()
        .position(is_it)
        .ok_or("The mod is no longer installed.")?;
    sidecar::move_mod(&paths.addons, &mods, index, sidecar::Move::Top)
        .map_err(|e| format!("Could not change the load order. {e}"))
}
