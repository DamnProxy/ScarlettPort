//! The native modules, each enabled by name through `VICTORY_NATIVE`.
//!
//! This tree ships the patcher only: no module is registered here, so
//! `VICTORY_NATIVE` does nothing until one is added.

/// Every module `install` knows.
pub static ALL: &[crate::NativeModule] = &[];
