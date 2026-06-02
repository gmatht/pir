use std::error::Error as StdError;

/// Type alias for boxed backend errors
pub type BackendError = Box<dyn StdError + Send + Sync>;

/// Backend application abstraction. Concrete backends provide an implementor boxed via `init()`.
pub trait BackendApp {
    /// Run the backend main loop. Consumes the backend app.
    fn run(self: Box<Self>) -> Result<(), BackendError>;
}

#[cfg(all(target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
pub mod gtk;
#[cfg(all(target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
pub use self::gtk::init;

#[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
pub mod nwg;
#[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
pub use self::nwg::init;

#[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
pub mod wasm;
#[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
pub use self::wasm::init;

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub mod android;
#[cfg(all(target_os = "android", not(feature = "zork")))]
pub use self::android::init_backend as init;

#[cfg(feature = "pancurses")]
pub mod pancurses;
#[cfg(feature = "pancurses")]
pub use self::pancurses::init;

#[cfg(feature = "zork")]
pub mod zork;
#[cfg(feature = "zork")]
pub use self::zork::init;
