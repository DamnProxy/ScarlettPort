//! Environment switches: every `VICTORY_*` variable can also be given as
//! `SCARLETT_*`. The `SCARLETT_` name is read first; `VICTORY_` is the
//! fallback and keeps working.

use std::env::VarError;
use std::ffi::OsString;

fn alias(name: &str) -> Option<String> {
    name.strip_prefix("VICTORY_").map(|rest| format!("SCARLETT_{rest}"))
}

/// `std::env::var`, trying the `SCARLETT_` spelling of a `VICTORY_` name first.
pub fn var(name: &str) -> Result<String, VarError> {
    if let Some(a) = alias(name) {
        match std::env::var(a) {
            Err(VarError::NotPresent) => {}
            other => return other,
        }
    }
    std::env::var(name)
}

/// `std::env::var_os`, trying the `SCARLETT_` spelling first.
pub fn var_os(name: &str) -> Option<OsString> {
    alias(name)
        .and_then(std::env::var_os)
        .or_else(|| std::env::var_os(name))
}

/// The variable's value when it is set (the name `env(name)` of the spec).
pub fn env(name: &str) -> Option<String> {
    var(name).ok()
}

/// At startup, before any thread: every `SCARLETT_*` variable is copied to
/// its `VICTORY_*` name (the `SCARLETT_` value wins), so code reading
/// `std::env` directly sees it too.
pub fn alias_scarlett() {
    let pairs: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let rest = k.to_str()?.strip_prefix("SCARLETT_")?;
            Some((OsString::from(format!("VICTORY_{rest}")), v))
        })
        .collect();
    for (k, v) in pairs {
        // SAFETY: called first thing in `main`, before other threads exist.
        unsafe { std::env::set_var(k, v) };
    }
}
