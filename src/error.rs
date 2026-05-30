use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("no usable GTK found (tried GTK4 then GTK3)")]
    NoGtkFound,
    #[error("dlopen failed for {lib}: {err}")]
    DlOpenFailed { lib: String, err: String },
    #[error("required symbol missing: {0}")]
    MissingSymbol(String),
    #[error("other error: {0}")]
    Other(String),
}

impl From<String> for Error {
    fn from(s: String) -> Self { Error::Other(s) }
}
