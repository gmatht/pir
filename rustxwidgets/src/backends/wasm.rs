#[cfg(target_arch = "wasm32")]
mod wasm_backend {
    use std::error::Error as StdError;

    pub struct WasmApp;

    impl crate::backends::BackendApp for WasmApp {
        fn run(self: Box<Self>) -> Result<(), Box<dyn StdError + Send + Sync>> {
            Ok(())
        }
    }

    pub fn init() -> Result<Box<dyn crate::backends::BackendApp>, Box<dyn StdError + Send + Sync>> {
        Ok(Box::new(WasmApp))
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm_backend::init;
