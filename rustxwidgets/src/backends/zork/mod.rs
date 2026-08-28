//! Headless `zork` backend for rustxwidgets.
//!
//! The backend is split into three pieces:
//!
//! * [`model`] — a pure, I/O-free in-memory model of the widget
//!   (`ZorkState` / `ZorkNode` / `ZorkKind`) plus all create/set/get/fire
//!   operations. This is the single source of truth shared by every driver.
//! * [`repl`] — the interactive text REPL (`ZorkApp`), kept for manual
//!   exploration and demos. It is *one* driver over the model.
//! * [`harness`] — a typed, synchronous in-process test API. This is the
//!   recommended replacement for a JSON backend: it exercises the very same
//!   model without stringly-typed commands, parsing, or serialization.
//!
//! The [`facade`] submodule re-exports the old free-function API
//! (`create_window`, `set_label_text`, …) over a thread-local model so the
//! existing `backends_zork_adapter` shim keeps compiling unchanged. New code
//! should use [`harness`] or [`model`] directly.
//!
//! JSON only earns its keep at a process boundary (snapshot diffs, external
//! non-Rust drivers). See [`model::ZorkState::snapshot`].

pub mod facade;
pub mod harness;
pub mod model;
pub mod repl;

// Re-export the free-function facade's items at the `zork` module root so the
// adapter (`crate::backends_zork_adapter`) resolves `crate::backends::zork::*`.
pub use facade::*;

pub use model::{Callback, MenuItemData, ZorkKind, ZorkNode, ZorkState};

use crate::backends::BackendApp;

/// Construct the interactive REPL driver over a fresh model.
pub fn init() -> Result<Box<dyn BackendApp>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Box::new(repl::ZorkApp::new()))
}
