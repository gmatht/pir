
#![feature(optimize_attribute)]

pub mod capture;
pub mod core;
pub mod addr;
pub mod agg;
pub mod balance;
pub mod celladdr;
pub mod export;
pub mod formula;
pub mod extrapolate;
pub mod grid;
pub mod io;
pub mod ods;
pub mod ops;
pub mod debug_log;
pub mod ui_core;
pub use ui_core::format_cell_display;

#[cfg(feature = "ratatui")]
pub mod ui;
#[cfg(any(feature = "gui", feature = "pancurses"))]
pub mod gui;
#[cfg(feature = "rustxwidgets-term")]
pub mod rustxwidgets_term;
