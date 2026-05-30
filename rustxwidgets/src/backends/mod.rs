use std::error::Error as StdError;

/// Type alias for boxed backend errors
pub type BackendError = Box<dyn StdError + Send + Sync>;

/// Backend application abstraction. Concrete backends provide an implementor boxed via `init()`.
pub trait BackendApp {
    /// Run the backend main loop. Consumes the backend app.
    fn run(self: Box<Self>) -> Result<(), BackendError>;
}

#[cfg(target_os = "linux")]
pub mod gtk;
#[cfg(target_os = "linux")]
pub use self::gtk::init;

#[cfg(windows)]
pub mod nwg;
#[cfg(windows)]
pub use self::nwg::init;
