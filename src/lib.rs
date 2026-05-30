//! gtk_dynamic_loader: runtime GTK loader and small safe wrappers (Linux-only)
#![allow(dead_code)]
mod error;
mod loader;
mod symbols;
mod wrappers;
mod signals;

pub use error::Error;
pub use loader::{Loader, Version};
pub use wrappers::{Application, Button, Label, Window, BoxWidget, Orientation, Grid, Entry, measure_text_px, DrawingArea, create_css_provider, add_provider_to_widget, Overlay, widget_set_size_request, widget_set_margin_start, widget_set_margin_top, destroy_widget};
// Re-export connection helpers so examples can attach to low-level GObject signals
pub use signals::{connect_signal_param, connect_signal_bool, connect_signal};
