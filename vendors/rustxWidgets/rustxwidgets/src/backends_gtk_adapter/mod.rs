// Re-export all adapter functions and types.
// Using glob to stay in sync with backends_gtk_adapter_impl automatically.
#[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
pub use crate::backends_gtk_adapter_impl::*;
