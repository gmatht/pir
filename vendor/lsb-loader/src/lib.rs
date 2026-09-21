//! Generic runtime dynamic loader for system libraries.
//!
//! On Unix, performs name-based candidate resolution with trust checks via `dladdr`.
//! On Windows, provides basic `LoadLibrary`-based loading via `libloading`.

use libloading::Library;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::os::raw::c_void;
use thiserror::Error;
#[cfg(unix)]
use libc;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[derive(Error, Debug)]
pub enum LoaderError {
    #[error("dlopen failed: {0}")]
    Dlopen(String),
    #[error("missing symbol: {0}")]
    MissingSymbol(String),
    #[cfg(unix)]
    #[error("trust check failed for {0}")]
    TrustCheckFailed(String),
    #[error("other: {0}")]
    Other(String),
}

pub struct LoadedLibrary {
    lib: Library,
    path: PathBuf,
}

impl LoadedLibrary {
    /// Attempt to load one of the candidate sonames and resolve required symbols.
    /// Supported on Unix only; returns `Err` on other platforms.
    pub fn load_from_candidates(
        candidates: &[&str],
        required: &[&str],
    ) -> Result<Self, LoaderError> {
        #[cfg(unix)]
        {
            Self::load_from_candidates_impl(candidates, required)
        }
        #[cfg(not(unix))]
        {
            let _ = (candidates, required);
            Err(LoaderError::Other("load_from_candidates is Unix-only".into()))
        }
    }

    #[cfg(unix)]
    fn load_from_candidates_impl(
        candidates: &[&str],
        required: &[&str],
    ) -> Result<Self, LoaderError> {
        for &soname in candidates {
            match unsafe { Library::new(soname) } {
                Ok(lib) => {
                    let mut missing = Vec::new();
                    for &sym in required {
                        unsafe {
                            let c = CString::new(sym).map_err(|e| LoaderError::Other(format!("bad symbol name: {}", e)))?;
                            let res = lib.get::<*const c_void>(c.as_bytes_with_nul());
                            if res.is_err() {
                                missing.push(sym);
                            }
                        }
                    }
                    if !missing.is_empty() {
                        drop(lib);
                        continue;
                    }

                    let path = match Self::resolved_path_via_dladdr(&lib, required[0]) {
                        Ok(p) => p,
                        Err(_) => PathBuf::from(soname),
                    };

                    if !Self::is_trusted_path(&path) {
                        drop(lib);
                        continue;
                    }

                    return Ok(LoadedLibrary { lib, path });
                }
                Err(e) => {
                    let _ = e;
                    continue;
                }
            }
        }
        Err(LoaderError::Dlopen("no suitable candidate found".into()))
    }

    #[cfg(unix)]
    fn resolved_path_via_dladdr(lib: &Library, sym: &str) -> Result<PathBuf, LoaderError> {
        unsafe {
            let c = CString::new(sym).map_err(|e| LoaderError::Other(format!("bad symbol name: {}", e)))?;
            let s = lib.get::<*const c_void>(c.as_bytes_with_nul()).map_err(|e| LoaderError::Other(format!("symbol lookup failed: {}", e)))?;
            let ptr = *s;
            let mut info: libc::Dl_info = std::mem::zeroed();
            let rv = libc::dladdr(ptr as *const c_void, &mut info as *mut libc::Dl_info);
            if rv == 0 {
                return Err(LoaderError::Other("dladdr failed".into()));
            }
            if info.dli_fname.is_null() {
                return Err(LoaderError::Other("dladdr returned null fname".into()));
            }
            let cstr = CStr::from_ptr(info.dli_fname);
            let s = cstr.to_string_lossy().into_owned();
            Ok(PathBuf::from(s))
        }
    }

    #[cfg(unix)]
    fn is_trusted_path(p: &Path) -> bool {
        if let Ok(abs) = p.canonicalize() {
            let s = abs.to_string_lossy();
            if s.starts_with("/lib") || s.starts_with("/usr/lib") || s.starts_with("/usr/local/lib") {
                if let Ok(meta) = fs::metadata(&abs) {
                    let mode = meta.mode();
                    if mode & 0o002 != 0 {
                        return false;
                    }
                    return true;
                }
            }
        }
        false
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load from an explicit path.
    pub fn load_explicit(path: &str, required: &[&str]) -> Result<Self, LoaderError> {
        #[cfg(unix)]
        {
            Self::load_explicit_impl(path, required)
        }
        #[cfg(not(unix))]
        {
            let p = Path::new(path);
            match unsafe { Library::new(p) } {
                Ok(lib) => {
                    let mut missing = Vec::new();
                    for &sym in required {
                        unsafe {
                            let c = CString::new(sym)
                                .map_err(|e| LoaderError::Other(format!("bad symbol name: {}", e)))?;
                            let res = lib.get::<*const c_void>(c.as_bytes_with_nul());
                            if res.is_err() {
                                missing.push(sym);
                            }
                        }
                    }
                    if !missing.is_empty() {
                        drop(lib);
                        return Err(LoaderError::MissingSymbol(format!(
                            "required symbols missing in {}: {:?}",
                            path, missing
                        )));
                    }
                    Ok(LoadedLibrary { lib, path: PathBuf::from(path) })
                }
                Err(e) => Err(LoaderError::Dlopen(format!("{}: {}", path, e))),
            }
        }
    }

    #[cfg(unix)]
    fn load_explicit_impl(path: &str, required: &[&str]) -> Result<Self, LoaderError> {
        let p = Path::new(path);
        if !p.is_absolute() {
            return Err(LoaderError::Other("explicit path must be absolute".into()));
        }
        if let Ok(meta) = fs::metadata(p) {
            if !meta.is_file() {
                return Err(LoaderError::Other(format!("not a regular file: {}", path)));
            }
            if meta.mode() & 0o002 != 0 {
                return Err(LoaderError::Other(format!("world-writable: {}", path)));
            }
        } else {
            return Err(LoaderError::Other(format!("cannot stat: {}", path)));
        }
        match unsafe { Library::new(path) } {
            Ok(lib) => {
                let mut missing = Vec::new();
                for &sym in required {
                    unsafe {
                        let c = CString::new(sym)
                            .map_err(|e| LoaderError::Other(format!("bad symbol name: {}", e)))?;
                        let res = lib.get::<*const c_void>(c.as_bytes_with_nul());
                        if res.is_err() {
                            missing.push(sym);
                        }
                    }
                }
                if !missing.is_empty() {
                    drop(lib);
                    return Err(LoaderError::MissingSymbol(format!(
                        "required symbols missing in {}: {:?}",
                        path, missing
                    )));
                }
                let resolved = Self::resolved_path_via_dladdr(&lib, required[0])
                    .unwrap_or_else(|_| PathBuf::from(path));
                Ok(LoadedLibrary { lib, path: resolved })
            }
            Err(e) => Err(LoaderError::Dlopen(format!("{}: {}", path, e))),
        }
    }

    /// Load from an explicit path with RTLD_GLOBAL so symbols are visible to transitive deps.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn load_explicit_global(path: &str, required: &[&str]) -> Result<Self, LoaderError> {
        use libloading::os::unix::Library as UnixLibrary;
        let p = Path::new(path);
        if !p.is_absolute() {
            return Err(LoaderError::Other("explicit path must be absolute".into()));
        }
        if let Ok(meta) = fs::metadata(p) {
            if !meta.is_file() {
                return Err(LoaderError::Other(format!("not a regular file: {}", path)));
            }
            if meta.mode() & 0o002 != 0 {
                return Err(LoaderError::Other(format!("world-writable: {}", path)));
            }
        } else {
            return Err(LoaderError::Other(format!("cannot stat: {}", path)));
        }
        match unsafe { UnixLibrary::open(Some(path), libc::RTLD_NOW | libc::RTLD_GLOBAL) } {
            Ok(unix_lib) => {
                let mut missing = Vec::new();
                for &sym in required {
                    unsafe {
                        let c = CString::new(sym)
                            .map_err(|e| LoaderError::Other(format!("bad symbol name: {}", e)))?;
                        let res = unix_lib.get::<*const c_void>(c.as_bytes_with_nul());
                        if res.is_err() {
                            missing.push(sym);
                        }
                    }
                }
                if !missing.is_empty() {
                    drop(unix_lib);
                    return Err(LoaderError::MissingSymbol(format!(
                        "required symbols missing in {}: {:?}",
                        path, missing
                    )));
                }
                let lib: Library = unix_lib.into();
                let resolved = Self::resolved_path_via_dladdr(&lib, required[0])
                    .unwrap_or_else(|_| PathBuf::from(path));
                Ok(LoadedLibrary { lib, path: resolved })
            }
            Err(e) => Err(LoaderError::Dlopen(format!("{}: {}", path, e))),
        }
    }

    /// Obtain a raw symbol pointer. Caller must ensure lifetime of library.
    pub unsafe fn get_symbol_raw(&self, name: &str) -> Result<*const c_void, LoaderError> {
        let c = CString::new(name).map_err(|e| LoaderError::Other(format!("bad symbol name: {}", e)))?;
        let sym = self.lib.get::<*const c_void>(c.as_bytes_with_nul()).map_err(|e| LoaderError::MissingSymbol(format!("{}: {}", name, e)))?;
        Ok(*sym)
    }
}
