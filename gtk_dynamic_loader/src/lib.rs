//! gtk_dynamic_loader: runtime GTK loader and small safe wrappers (Linux-only)
#![allow(dead_code)]
mod error;
mod loader;
mod symbols;
mod wrappers;
mod signals;

pub use error::Error;
pub use loader::{Loader, Version};
pub use wrappers::{Application, Button, Label, Window, BoxWidget, Orientation, Grid, Entry, Menu, MenuBar, SimpleAction, measure_text_px, DrawingArea, create_css_provider, add_provider_to_widget, add_css_provider_global, Overlay, widget_set_size_request, widget_set_margin_start, widget_set_margin_top, widget_set_hexpand, widget_set_vexpand, widget_set_halign, widget_set_valign, widget_set_can_target, widget_set_visible, widget_unparent, widget_queue_draw, widget_connect_signal_bool, connect_gesture_click_pressed, gdk_event_get_coords, destroy_widget, unref_widget, remove_from_parent, take_ownership, Dialog, DropDown, CheckButton, RadioButton, TextView, ScrolledWindow, GestureClick, EventControllerKey, FileChooserNative, CairoContext, CairoTextExtents};
// Re-export connection helpers so examples can attach to low-level GObject signals
pub use signals::{connect_signal_param, connect_signal_bool, connect_signal, connect_signal_gesture, connect_signal_motion, gtk_compat_trampoline_draw_gtk3, gtk_compat_destroy_notify_draw_gtk3, gtk_compat_trampoline_draw_gtk4, gtk_compat_destroy_notify_draw_gtk4, gtk_compat_trampoline_key_pressed, gtk_compat_destroy_notify_key_pressed};
