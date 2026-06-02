//! rustxwidgets: cross-platform thin GUI abstraction (GTK-dlopen on Linux, NWG on Windows)
#![warn(missing_docs)]

pub mod prelude;
pub mod core;
pub mod overflow;
pub mod lifecycle_stress;
pub mod backends;
#[cfg(all(target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
mod backends_gtk_adapter_impl;
#[cfg(all(not(feature = "pancurses"), not(feature = "zork")))]
pub mod backends_gtk_adapter;
#[cfg(all(windows, not(feature = "zork")))]
pub mod backends_nwg_adapter;
#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
pub mod backends_wasm_adapter;
#[cfg(all(target_os = "android", not(feature = "zork")))]
pub mod backends_android_adapter;
#[cfg(feature = "pancurses")]
pub mod backends_pancurses_adapter;
#[cfg(feature = "zork")]
pub mod backends_zork_adapter;

pub use prelude::*;
