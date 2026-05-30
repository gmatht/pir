//! rustxwidgets: cross-platform thin GUI abstraction (GTK-dlopen on Linux, NWG on Windows)
#![warn(missing_docs)]

pub mod prelude;
pub mod core;
pub mod overflow;
pub mod backends;
#[cfg(target_os = "linux")]
mod backends_gtk_adapter_impl;
pub mod backends_gtk_adapter;

pub use prelude::*;
