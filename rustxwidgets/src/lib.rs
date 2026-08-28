//! rustxwidgets: cross-platform thin GUI abstraction (GTK-dlopen on Linux, NWG on Windows)
#![warn(missing_docs)]

pub mod prelude;
pub mod core;
pub mod spreadsheet;
pub mod overflow;

/// Re-export the dynamic GTK loader so host apps using the `gtk` backend can reach
/// raw symbols (signal wiring, event state, ...) without depending on it directly.
#[cfg(feature = "gtk")]
pub use gtk_dynamic_loader;
pub mod lifecycle_stress;
pub mod backends;
#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
pub mod backends_gtk_adapter {
    // The gtk4 adapter uses dlopen-loaded sys crates via gtk4-rs.
    // Source is in backends_gtk4_adapter.rs
    include!("backends_gtk4_adapter.rs");
}
#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
mod backends_gtk_adapter_impl;
#[cfg(all(feature = "gtk", not(feature = "pancurses"), not(feature = "zork")))]
pub mod backends_gtk_adapter;
#[cfg(all(windows, not(feature = "zork")))]
pub mod backends_nwg_adapter;
#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
pub mod backends_wasm_adapter;
#[cfg(all(target_os = "android", not(feature = "zork")))]
pub mod backends_android_adapter;
#[cfg(feature = "pancurses")]
pub mod backends_pancurses_adapter;
#[cfg(feature = "pancurses")]
pub use crate::backends::pancurses::set_frame_hook;
#[cfg(feature = "zork")]
pub mod backends_zork_adapter;

pub use prelude::*;
