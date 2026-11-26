//! `VICTORY_LOOK=1`: every picture switch of the back end ported from
//! cool_fox (codec-d/cool_fox, `fx-coolfox`) on, at the value cool_fox runs
//! with (its Play-VIctory.cmd, or its code default). Off (the default), each
//! switch is off unless set on its own. A switch set on its own always wins.
//! The switches it covers are the callers of [`var`] and [`Live`].

use std::sync::OnceLock;

/// Whether the umbrella is on (`VICTORY_LOOK`, boot).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_LOOK")
            .ok()
            .and_then(|v| victory_core::live::parse(&v))
            .is_some_and(|v| v != 0);
        if on {
            tracing::info!("look switches are on (VICTORY_LOOK=1)");
        }
        on
    })
}

/// `VICTORY_<name>` as the environment gives it, or `cf` (cool_fox's value)
/// when it is unset and the umbrella is on.
pub(crate) fn var(name: &str, cf: &str) -> Option<String> {
    match std::env::var(format!("VICTORY_{name}")) {
        Ok(v) => Some(v),
        Err(_) => on().then(|| cf.to_owned()),
    }
}

/// A live switch ([`victory_core::live::Switch`]) whose default is 0, or
/// cool_fox's value `cf` under the umbrella.
pub(crate) struct Live {
    name: &'static str,
    cf: u64,
    cell: OnceLock<victory_core::live::Switch>,
}

impl Live {
    pub(crate) const fn new(name: &'static str, cf: u64) -> Self {
        Live { name, cf, cell: OnceLock::new() }
    }

    #[inline]
    pub(crate) fn get(&self) -> u64 {
        self.cell
            .get_or_init(|| victory_core::live::Switch::new(self.name, if on() { self.cf } else { 0 }))
            .get()
    }

    #[inline]
    pub(crate) fn on(&self) -> bool {
        self.get() != 0
    }
}

/// Upstream graphics state fixes (`VICTORY_STATE_FIXES`, boot; off, on under
/// the umbrella): sampler bounds, appended vertex elements aligned, signed
/// rectangle attributes, A8 apart from R8, depth-stencil copy footprints.
pub(crate) fn state_fixes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| boot("STATE_FIXES", 1) != 0)
}

/// A boot switch: `VICTORY_<name>` as a number, or `cf` under the umbrella,
/// or 0.
pub(crate) fn boot(name: &str, cf: u64) -> u64 {
    var(name, &cf.to_string()).and_then(|v| victory_core::live::parse(&v)).unwrap_or(0)
}
