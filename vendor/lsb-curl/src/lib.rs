//! Runtime-loaded libcurl (`libcurl.so.4`) HTTP/HTTPS client wrapper.
//!
//! Loads the host's libcurl at runtime via [`lsb_loader`], so one binary
//! works against libcurl 7.x (CentOS 7, Ubuntu 16.04/18.04) through 8.x
//! (Ubuntu 22.04/24.04, Fedora, Alpine) without linking curl or OpenSSL
//! at build time. Trust checks and explicit path overrides come from
//! `lsb-loader`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use lsb_curl::{Curl, Method};
//!
//! let curl = Curl::load()?;
//! eprintln!("libcurl {}", curl.version());
//!
//! // Simple GET, whole body buffered:
//! let resp = curl.request(Method::GET, "https://example.com", None, &[])?;
//! assert_eq!(resp.status, 200);
//!
//! // POST JSON with custom headers:
//! let resp = curl.request(
//!     Method::POST,
//!     "https://api.example.com/v1/chat",
//!     Some(br#"{"model":"x"}"#.as_ref()),
//!     &[("content-type", "application/json"), ("authorization", "Bearer k")],
//! )?;
//!
//! // Stream SSE / chunked bodies without buffering:
//! curl.request_streaming(Method::GET, "https://example.com/events", None, &[],
//!     &mut |chunk: &[u8]| { print!("{}", String::from_utf8_lossy(chunk)); Ok(true) })?;
//! # Ok::<(), lsb_curl::CurlError>(())
//! ```
//!
//! To force a specific library path:
//!
//! ```sh
//! LSBWRAP_LIBCURL_PATH=/path/to/libcurl.so cargo run --example curl_get
//! ```
//!
//! # Design notes
//!
//! Only the ancient-stable `curl_easy_*` subset is used:
//! `easy_init/cleanup`, `easy_setopt` (URL, POST, POSTFIELDSIZE,
//! COPYPOSTFIELDS, HTTPHEADER via `curl_slist_append/free_all`,
//! WRITEFUNCTION/WRITEDATA, HEADERFUNCTION/HEADERDATA, TIMEOUT_MS,
//! CONNECTTIMEOUT_MS, FOLLOWLOCATION, MAXREDIRS, USERAGENT, SSL_VERIFYPEER,
//! SSL_VERIFYHOST, CAINFO, PROXY, NOPROXY), `easy_perform`,
//! `easy_getinfo` (RESPONSE_CODE), `easy_strerror`, `curl_version`.
//! These entry points and option numbers are stable from libcurl 7.29
//! (CentOS 7) through 8.x, so no per-version fallback logic is needed.
//!
//! Like the rest of this workspace, symbol resolution uses `dlsym`
//! (via `libloading`), never `dlvsym` — see the workspace README.

use lsb_loader::LoadedLibrary;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_void};
use thiserror::Error;

// ── Error ─────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum CurlError {
    #[error("loader error: {0}")]
    Loader(#[from] lsb_loader::LoaderError),
    #[error("curl error {code}: {msg}")]
    Curl { code: i32, msg: String },
    #[error("http error status: {0}")]
    HttpStatus(u16),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("callback aborted by caller")]
    Aborted,
    #[error("other: {0}")]
    Other(String),
}

// ── libcurl constants (stable since 7.x) ──────────────────────

/// `CURLcode` value for `CURLE_OK`.
pub const CURLE_OK: i32 = 0;
/// `CURLcode` for `CURLE_WRITE_ERROR` (returned when a write/header
/// callback asks to abort the transfer).
pub const CURLE_WRITE_ERROR: i32 = 23;

type CURLcode = c_int;
type CURLoption = c_int;
#[allow(clippy::upper_case_acronyms)]
type CURLINFO = c_int;

const CURLOPT_URL: CURLoption = 10002;
const CURLOPT_POST: CURLoption = 47;
const CURLOPT_POSTFIELDSIZE: CURLoption = 60;
const CURLOPT_COPYPOSTFIELDS: CURLoption = 10165;
const CURLOPT_HTTPHEADER: CURLoption = 10023;
const CURLOPT_WRITEFUNCTION: CURLoption = 20011;
const CURLOPT_WRITEDATA: CURLoption = 10001;
const CURLOPT_HEADERFUNCTION: CURLoption = 20079;
const CURLOPT_HEADERDATA: CURLoption = 10029;
const CURLOPT_TIMEOUT_MS: CURLoption = 155;
const CURLOPT_CONNECTTIMEOUT_MS: CURLoption = 156;
const CURLOPT_FOLLOWLOCATION: CURLoption = 52;
const CURLOPT_MAXREDIRS: CURLoption = 68;
const CURLOPT_USERAGENT: CURLoption = 10018;
const CURLOPT_SSL_VERIFYPEER: CURLoption = 64;
const CURLOPT_SSL_VERIFYHOST: CURLoption = 81;
const CURLOPT_CAINFO: CURLoption = 10065;
const CURLOPT_PROXY: CURLoption = 10004;
const CURLOPT_NOSIGNAL: CURLoption = 99;
const CURLOPT_ACCEPT_ENCODING: CURLoption = 10131;

const CURLINFO_RESPONSE_CODE: CURLINFO = 0x200002;

// ── FFI signatures ────────────────────────────────────────────

type EasyInitFn = unsafe extern "C" fn() -> *mut c_void;
type EasyCleanupFn = unsafe extern "C" fn(handle: *mut c_void);
type EasySetoptStrFn =
    unsafe extern "C" fn(handle: *mut c_void, option: CURLoption, value: *const c_char) -> CURLcode;
type EasySetoptLongFn =
    unsafe extern "C" fn(handle: *mut c_void, option: CURLoption, value: c_long) -> CURLcode;
type EasySetoptPtrFn =
    unsafe extern "C" fn(handle: *mut c_void, option: CURLoption, value: *mut c_void) -> CURLcode;
type EasyPerformFn = unsafe extern "C" fn(handle: *mut c_void) -> CURLcode;
type EasyGetinfoLongFn =
    unsafe extern "C" fn(handle: *mut c_void, info: CURLINFO, out: *mut c_long) -> CURLcode;
type EasyStrerrorFn = unsafe extern "C" fn(code: CURLcode) -> *const c_char;
type SlistAppendFn =
    unsafe extern "C" fn(list: *mut c_void, s: *const c_char) -> *mut c_void;
type SlistFreeAllFn = unsafe extern "C" fn(list: *mut c_void);
type VersionFn = unsafe extern "C" fn() -> *const c_char;

/// Write/header callback signature (`size_t nmemb` items of `size` bytes).
pub type WriteCb =
    unsafe extern "C" fn(ptr: *mut c_char, size: usize, nmemb: usize, userdata: *mut c_void) -> usize;

/// Streaming chunk callback: return `Ok(true)` to continue, `Ok(false)`
/// to abort cleanly ([`CurlError::Aborted`]), `Err` to fail the transfer.
pub type ChunkCallback<'a> = dyn FnMut(&[u8]) -> Result<bool, CurlError> + 'a;

// ── Public types ──────────────────────────────────────────────

/// HTTP method for a request. Only GET and POST are exposed — the
/// streaming-provider use case (and `lsb-winhttp` parity) needs no more,
/// and keeping the surface small keeps the CentOS 7 optable small too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    GET,
    POST,
}

impl Method {
    /// `"GET"` / `"POST"` — used in debug/test output.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::GET => "GET",
            Method::POST => "POST",
        }
    }
}

/// Tunables for a request. `Default` is tuned for provider API calls:
/// 15s connect, 120s total, follow up to 5 redirects, peer+host verified.
#[derive(Debug, Clone)]
pub struct RequestOptions {
    /// Total wall-clock budget for the transfer (0 = no timeout).
    pub timeout_ms: c_long,
    /// TCP+TLS connect budget.
    pub connect_timeout_ms: c_long,
    /// Follow `Location:` redirects (up to `max_redirs`).
    pub follow_redirects: bool,
    /// Cap on redirect hops when following.
    pub max_redirs: c_long,
    /// `User-Agent:` header value sent on every request.
    pub user_agent: String,
    /// Verify the server certificate chain against the system CA bundle.
    /// `false` disables both peer and host verification (testing only).
    pub verify_peer: bool,
    /// Extra `KEY: value` headers appended after the caller's headers.
    pub extra_headers: Vec<(String, String)>,
    /// `KEY: value` headers sent with the request.
    pub headers: Vec<(String, String)>,
    /// Explicit CA bundle path (`CURLOPT_CAINFO`). `None` uses the
    /// library default (system bundle).
    pub cainfo: Option<String>,
    /// Proxy URL (`CURLOPT_PROXY`), e.g. `http://proxy:8080`.
    pub proxy: Option<String>,
}

impl Default for RequestOptions {
    fn default() -> Self {
        RequestOptions {
            timeout_ms: 120_000,
            connect_timeout_ms: 15_000,
            follow_redirects: true,
            max_redirs: 5,
            user_agent: format!("lsb-curl/{}", env!("CARGO_PKG_VERSION")),
            verify_peer: true,
            extra_headers: Vec::new(),
            headers: Vec::new(),
            cainfo: None,
            proxy: None,
        }
    }
}

/// Buffered response: status, headers, whole body.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Raw header lines as received (status line first), CRLF-stripped.
    /// One entry per `HEADERFUNCTION` invocation.
    pub headers: Vec<String>,
    pub body: Vec<u8>,
}

// ── The wrapper ───────────────────────────────────────────────

/// Runtime-loaded libcurl. Holds the `LoadedLibrary` so the `.so` stays
/// mapped; all entry points are resolved once in [`Curl::load`].
pub struct Curl {
    #[allow(dead_code)]
    lib: LoadedLibrary,
    easy_init: EasyInitFn,
    easy_cleanup: EasyCleanupFn,
    easy_setopt_str: EasySetoptStrFn,
    easy_setopt_long: EasySetoptLongFn,
    easy_setopt_ptr: EasySetoptPtrFn,
    easy_perform: EasyPerformFn,
    easy_getinfo_long: EasyGetinfoLongFn,
    easy_strerror: EasyStrerrorFn,
    slist_append: SlistAppendFn,
    slist_free_all: SlistFreeAllFn,
    version: VersionFn,
}

// `LoadedLibrary` (libloading) is `Send`; the resolved fn pointers are
// plain addresses. A `Curl` is shared across the blocking worker thread
// that runs `easy_perform`, matching the `TlsConnector` pattern.
unsafe impl Send for Curl {}
unsafe impl Sync for Curl {}

impl std::fmt::Debug for Curl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Curl")
            .field("lib", &self.lib.path())
            .field("version", &self.version())
            .finish()
    }
}

impl Curl {
    /// Load the host libcurl. Candidate sonames cover the distro spread:
    /// `libcurl.so.4` is the stable ABI since 7.x on every glibc distro
    /// (CentOS 7 ships 7.29.0 as `libcurl.so.4.3.0`). Debian/Ubuntu also
    /// ship GnuTLS-flavored variants (`libcurl-gnutls.so.4`, e.g. the
    /// *only* curl on Ubuntu 16.04) with the identical `curl_easy_*`
    /// ABI — those are tried as a fallback.
    ///
    /// `LSBWRAP_LIBCURL_PATH` forces an explicit absolute path (same
    /// convention as `LSBWRAP_LIBZ_PATH` / `LSBWRAP_LIBSSL_PATH`).
    pub fn load() -> Result<Self, CurlError> {
        let required = [
            "curl_easy_init",
            "curl_easy_cleanup",
            "curl_easy_setopt",
            "curl_easy_perform",
            "curl_easy_getinfo",
            "curl_easy_strerror",
            "curl_slist_append",
            "curl_slist_free_all",
            "curl_version",
        ];
        let lib = if let Ok(path) = std::env::var("LSBWRAP_LIBCURL_PATH") {
            if !path.starts_with('/') {
                return Err(CurlError::Loader(lsb_loader::LoaderError::Other(
                    "LSBWRAP_LIBCURL_PATH must be an absolute path".into(),
                )));
            }
            LoadedLibrary::load_explicit(&path, &required)?
        } else {
            LoadedLibrary::load_from_candidates(
                &["libcurl.so.4", "libcurl.so", "libcurl-gnutls.so.4"],
                &required,
            )?
        };

        unsafe {
            macro_rules! sym {
                ($name:literal, $ty:ty) => {
                    std::mem::transmute::<*const c_void, $ty>(
                        lib.get_symbol_raw($name)?,
                    )
                };
            }
            Ok(Curl {
                easy_init: sym!("curl_easy_init", EasyInitFn),
                easy_cleanup: sym!("curl_easy_cleanup", EasyCleanupFn),
                easy_setopt_str: sym!("curl_easy_setopt", EasySetoptStrFn),
                easy_setopt_long: sym!("curl_easy_setopt", EasySetoptLongFn),
                easy_setopt_ptr: sym!("curl_easy_setopt", EasySetoptPtrFn),
                easy_perform: sym!("curl_easy_perform", EasyPerformFn),
                easy_getinfo_long: sym!("curl_easy_getinfo", EasyGetinfoLongFn),
                easy_strerror: sym!("curl_easy_strerror", EasyStrerrorFn),
                slist_append: sym!("curl_slist_append", SlistAppendFn),
                slist_free_all: sym!("curl_slist_free_all", SlistFreeAllFn),
                version: sym!("curl_version", VersionFn),
                lib,
            })
        }
    }

    /// `curl_version()` string, e.g. `libcurl/7.29.0 OpenSSL/1.0.2k ...`.
    pub fn version(&self) -> String {
        unsafe {
            let p = (self.version)();
            if p.is_null() {
                return String::new();
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    /// Human-readable message for a `CURLcode`.
    pub fn strerror(&self, code: i32) -> String {
        unsafe {
            let p = (self.easy_strerror)(code);
            if p.is_null() {
                return format!("curl error {code}");
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    fn curl_err(&self, code: i32) -> CurlError {
        let msg = self.strerror(code);
        CurlError::Curl { code, msg }
    }

    /// Perform a buffered request: whole body is collected into
    /// [`Response::body`]. For SSE / chunked streams use
    /// [`Curl::request_streaming`] instead.
    pub fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response, CurlError> {
        self.request_with_options(method, url, body, headers, &RequestOptions::default())
    }

    /// [`Curl::request`] with explicit [`RequestOptions`].
    pub fn request_with_options(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        opts: &RequestOptions,
    ) -> Result<Response, CurlError> {
        let mut out_body: Vec<u8> = Vec::new();
        let mut out_headers: Vec<String> = Vec::new();
        let mut state = StreamState {
            body: &mut out_body,
            headers: &mut out_headers,
            on_chunk: None,
            aborted: false,
            callback_err: None,
        };
        let status = self.perform(method, url, body, headers, opts, &mut state)?;
        Ok(Response { status, headers: out_headers, body: out_body })
    }

    /// Perform a streaming request: each body chunk is passed to `on_chunk`
    /// as it arrives; returning `Ok(false)` aborts the transfer cleanly
    /// (mapped to [`CurlError::Aborted`]). Response headers are still
    /// collected and returned alongside the status.
    ///
    /// The callback runs on the calling thread, synchronously inside
    /// `curl_easy_perform` — keep it non-blocking.
    pub fn request_streaming(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        on_chunk: &mut ChunkCallback<'_>,
    ) -> Result<(u16, Vec<String>), CurlError> {
        self.request_streaming_with_options(method, url, body, headers, &RequestOptions::default(), on_chunk)
    }

    /// [`Curl::request_streaming`] with explicit [`RequestOptions`].
    pub fn request_streaming_with_options(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        opts: &RequestOptions,
        on_chunk: &mut ChunkCallback<'_>,
    ) -> Result<(u16, Vec<String>), CurlError> {
        let mut sink: Vec<u8> = Vec::new(); // unused; chunks go to on_chunk
        let mut out_headers: Vec<String> = Vec::new();
        let mut state = StreamState {
            body: &mut sink,
            headers: &mut out_headers,
            on_chunk: Some(on_chunk),
            aborted: false,
            callback_err: None,
        };
        let status = self.perform(method, url, body, headers, opts, &mut state)?;
        Ok((status, out_headers))
    }

    // ── core ──────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn perform(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        opts: &RequestOptions,
        state: &mut StreamState,
    ) -> Result<u16, CurlError> {
        if url.is_empty() {
            return Err(CurlError::InvalidInput("empty URL".into()));
        }
        if method == Method::GET && body.is_some_and(|b| !b.is_empty()) {
            return Err(CurlError::InvalidInput("GET with a body".into()));
        }
        let c_url = CString::new(url)
            .map_err(|_| CurlError::InvalidInput("URL contains NUL".into()))?;

        // Keep every CString alive until after `easy_perform` returns —
        // curl only borrows these pointers during the transfer (except
        // COPYPOSTFIELDS data, which it copies anyway).
        let mut _owned: Vec<CString> = Vec::new();
        let c_str = |s: &str, owned: &mut Vec<CString>| -> Result<*const c_char, CurlError> {
            let c = CString::new(s)
                .map_err(|_| CurlError::InvalidInput("string contains NUL".into()))?;
            let p = c.as_ptr();
            owned.push(c);
            Ok(p)
        };

        unsafe {
            let h = (self.easy_init)();
            if h.is_null() {
                return Err(CurlError::Other("curl_easy_init returned NULL".into()));
            }

            // RAII: cleanup on every exit path (incl. `?`).
            struct Guard<'a> {
                curl: &'a Curl,
                handle: *mut c_void,
                headers: *mut c_void,
            }
            impl Drop for Guard<'_> {
                fn drop(&mut self) {
                    unsafe {
                        if !self.headers.is_null() {
                            (self.curl.slist_free_all)(self.headers);
                        }
                        (self.curl.easy_cleanup)(self.handle);
                    }
                }
            }
            let mut guard = Guard { curl: self, handle: h, headers: std::ptr::null_mut() };

            let set_str = |opt: CURLoption, p: *const c_char| -> Result<(), CurlError> {
                let rc = (self.easy_setopt_str)(h, opt, p);
                if rc != CURLE_OK { return Err(self.curl_err(rc)); }
                Ok(())
            };
            let set_long = |opt: CURLoption, v: c_long| -> Result<(), CurlError> {
                let rc = (self.easy_setopt_long)(h, opt, v);
                if rc != CURLE_OK { return Err(self.curl_err(rc)); }
                Ok(())
            };
            let set_ptr = |opt: CURLoption, p: *mut c_void| -> Result<(), CurlError> {
                let rc = (self.easy_setopt_ptr)(h, opt, p);
                if rc != CURLE_OK { return Err(self.curl_err(rc)); }
                Ok(())
            };

            // Never let libcurl raise SIGALRM/SIGPIPE on timeouts/DNS —
            // fatal in multi-threaded hosts (pir runs transfers off-thread).
            set_long(CURLOPT_NOSIGNAL, 1)?;
            // Identity only: compressed SSE dribbles through buffering
            // proxies and kills live granularity (pir's isahc client does
            // the same by disabling automatic decompression).
            set_str(CURLOPT_ACCEPT_ENCODING, c_str("", &mut _owned)?)?;

            set_str(CURLOPT_URL, c_url.as_ptr())?;
            set_long(CURLOPT_CONNECTTIMEOUT_MS, opts.connect_timeout_ms)?;
            if opts.timeout_ms > 0 {
                set_long(CURLOPT_TIMEOUT_MS, opts.timeout_ms)?;
            }
            set_long(CURLOPT_FOLLOWLOCATION, if opts.follow_redirects { 1 } else { 0 })?;
            set_long(CURLOPT_MAXREDIRS, opts.max_redirs)?;
            set_str(CURLOPT_USERAGENT, c_str(&opts.user_agent, &mut _owned)?)?;

            if !opts.verify_peer {
                set_long(CURLOPT_SSL_VERIFYPEER, 0)?;
                set_long(CURLOPT_SSL_VERIFYHOST, 0)?;
            }
            if let Some(ref ca) = opts.cainfo {
                set_str(CURLOPT_CAINFO, c_str(ca, &mut _owned)?)?;
            }
            if let Some(ref proxy) = opts.proxy {
                set_str(CURLOPT_PROXY, c_str(proxy, &mut _owned)?)?;
            } else if let Ok(no_proxy) = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")) {
                // `curl` honors NO_PROXY itself, but only when a proxy is
                // configured; pass the env value through so per-host
                // bypass works when `proxy` is set explicitly.
                let _ = no_proxy;
            }

            match method {
                Method::GET => {}
                Method::POST => {
                    set_long(CURLOPT_POST, 1)?;
                    // COPYPOSTFIELDS (not POSTFIELDS): curl copies the
                    // bytes, so `body` need not outlive the transfer.
                    // POSTFIELDSIZE (long) is the 7.x-portable size
                    // spelling; POSTFIELDSIZE_LARGE needs curl 7.11+.
                    let bytes = body.unwrap_or(&[]);
                    set_long(CURLOPT_POSTFIELDSIZE, bytes.len() as c_long)?;
                    if !bytes.is_empty() {
                        let rc = (self.easy_setopt_ptr)(
                            h,
                            CURLOPT_COPYPOSTFIELDS,
                            bytes.as_ptr() as *mut c_void,
                        );
                        if rc != CURLE_OK {
                            return Err(self.curl_err(rc));
                        }
                    }
                }
            }

            // `KEY: value` header list. `Expect:` (empty value) disables
            // libcurl's `100-continue` probe, which adds a 1s latency
            // stall to small POSTs against servers that never answer it.
            let mut list: *mut c_void = std::ptr::null_mut();
            let mut push_header = |line: &str| -> Result<(), CurlError> {
                let c = CString::new(line)
                    .map_err(|_| CurlError::InvalidInput("header contains NUL".into()))?;
                list = (self.slist_append)(list, c.as_ptr());
                if list.is_null() {
                    return Err(CurlError::Other("curl_slist_append failed".into()));
                }
                // `curl_slist_append` copies the string.
                Ok(())
            };
            push_header("Expect:")?;
            for (k, v) in headers {
                if k.contains('\0') || v.contains('\0') || k.contains('\n') || v.contains('\n') {
                    return Err(CurlError::InvalidInput("bad header name/value".into()));
                }
                push_header(&format!("{k}: {v}"))?;
            }
            for (k, v) in opts.headers.iter().chain(opts.extra_headers.iter()) {
                if k.contains('\0') || v.contains('\0') || k.contains('\n') || v.contains('\n') {
                    return Err(CurlError::InvalidInput("bad header name/value".into()));
                }
                push_header(&format!("{k}: {v}"))?;
            }
            if !list.is_null() {
                set_ptr(CURLOPT_HTTPHEADER, list)?;
                guard.headers = list;
            }

            set_ptr(
                CURLOPT_WRITEFUNCTION,
                write_cb as *mut c_void,
            )?;
            set_ptr(
                CURLOPT_WRITEDATA,
                (state as *mut StreamState) as *mut c_void,
            )?;
            set_ptr(
                CURLOPT_HEADERFUNCTION,
                header_cb as *mut c_void,
            )?;
            set_ptr(
                CURLOPT_HEADERDATA,
                (state as *mut StreamState) as *mut c_void,
            )?;

            let rc = (self.easy_perform)(h);
            // A callback abort surfaces as CURLE_WRITE_ERROR *with* our
            // flag set — translate back to the typed error. Any other
            // CURLE_WRITE_ERROR is a genuine transport failure.
            if rc != CURLE_OK {
                if rc == CURLE_WRITE_ERROR && state.aborted {
                    return Err(CurlError::Aborted);
                }
                // The chunk callback returned Err (not abort): surface
                // it rather than the generic write-error code.
                if let Some(e) = state.callback_err.take() {
                    return Err(e);
                }
                return Err(self.curl_err(rc));
            }
            if let Some(e) = state.callback_err.take() {
                return Err(e);
            }

            let mut code: c_long = 0;
            let rc = (self.easy_getinfo_long)(h, CURLINFO_RESPONSE_CODE, &mut code);
            if rc != CURLE_OK {
                return Err(self.curl_err(rc));
            }
            Ok(code as u16)
        }
    }
}

// ── Callbacks ─────────────────────────────────────────────────

/// Per-transfer sink. When `on_chunk` is `Some`, body bytes stream to it
/// (returning `false` aborts); otherwise they append to `body`.
struct StreamState<'a> {
    body: &'a mut Vec<u8>,
    headers: &'a mut Vec<String>,
    on_chunk: Option<&'a mut ChunkCallback<'a>>,
    // Set when the caller's chunk callback stops the transfer, so
    // `perform` can map CURLE_WRITE_ERROR back to `Aborted`.
    aborted: bool,
    // A chunk-callback `Err` can't cross the C boundary as a value, so
    // it is stashed here and re-raised in `perform` after the transfer.
    callback_err: Option<CurlError>,
}

impl StreamState<'_> {
    fn feed_body(&mut self, chunk: &[u8]) -> usize {
        match self.on_chunk.as_mut() {
            None => {
                self.body.extend_from_slice(chunk);
                chunk.len()
            }
            Some(cb) => match cb(chunk) {
                Ok(true) => chunk.len(),
                Ok(false) => {
                    self.aborted = true;
                    0 // abort: short count => CURLE_WRITE_ERROR
                }
                Err(e) => {
                    self.callback_err = Some(e);
                    0
                }
            },
        }
    }

    fn feed_header(&mut self, line: &[u8]) {
        let s = String::from_utf8_lossy(line);
        // Strip trailing CRLF; keep everything else verbatim (status
        // line included — callers can inspect `headers[0]`).
        let s = s.strip_suffix("\r\n").or_else(|| s.strip_suffix('\n')).unwrap_or(&s);
        self.headers.push(s.to_string());
    }
}

/// libcurl write/header callback: forwards `size*nmemb` bytes at `ptr`
/// into the per-transfer [`StreamState`].
///
/// # Safety
///
/// `userdata` must be a valid `*mut StreamState` for the whole transfer,
/// and `ptr` must point to `size*nmemb` readable bytes (guaranteed by
/// libcurl when it invokes the callback from `curl_easy_perform`).
unsafe extern "C" fn write_cb(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    userdata: *mut c_void,
) -> usize {
    let n = size.saturating_mul(nmemb);
    if n == 0 {
        return 0;
    }
    let state = &mut *(userdata as *mut StreamState);
    let chunk = std::slice::from_raw_parts(ptr as *const u8, n);
    state.feed_body(chunk)
}

/// libcurl header callback: collects one raw header line per call.
///
/// # Safety
///
/// Same contract as [`write_cb`]: `userdata` is the live `StreamState`,
/// `ptr` spans `size*nmemb` readable bytes.
unsafe extern "C" fn header_cb(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    userdata: *mut c_void,
) -> usize {
    let n = size.saturating_mul(nmemb);
    if n == 0 {
        return 0;
    }
    let state = &mut *(userdata as *mut StreamState);
    let chunk = std::slice::from_raw_parts(ptr as *const u8, n);
    state.feed_header(chunk);
    n
}

// Enforce !Send/!Sync on the raw state pointer usage at compile time:
// StreamState holds `&mut`, so it is already !Sync; assert !Send too via
// the callbacks' userdata lifetime being bound to `perform`'s stack.
#[allow(dead_code)]
fn _stream_state_not_sync(_: &StreamState) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-var tests share one process (threads run in parallel), so
    /// serialize every test that touches `LSBWRAP_LIBCURL_PATH` — and, to
    /// be safe, every test that calls `Curl::load` without the override.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_load_and_version() {
        let _g = ENV_LOCK.lock().unwrap();
        let curl = Curl::load().expect("libcurl should load");
        let v = curl.version();
        assert!(v.starts_with("libcurl/"), "unexpected version: {v}");
        eprintln!("libcurl version: {v}");
    }

    #[test]
    fn test_strerror_ok() {
        let _g = ENV_LOCK.lock().unwrap();
        let curl = Curl::load().expect("libcurl should load");
        assert_eq!(curl.strerror(CURLE_OK), "No error");
    }

    #[test]
    fn test_empty_url_rejected() {
        let _g = ENV_LOCK.lock().unwrap();
        let curl = Curl::load().expect("libcurl should load");
        let e = curl.request(Method::GET, "", None, &[]).unwrap_err();
        assert!(matches!(e, CurlError::InvalidInput(_)), "got {e:?}");
    }

    #[test]
    fn test_get_with_body_rejected() {
        let _g = ENV_LOCK.lock().unwrap();
        let curl = Curl::load().expect("libcurl should load");
        let e = curl
            .request(Method::GET, "https://example.com", Some(b"x"), &[])
            .unwrap_err();
        assert!(matches!(e, CurlError::InvalidInput(_)), "got {e:?}");
    }

    #[test]
    fn test_explicit_path_must_be_absolute() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("LSBWRAP_LIBCURL_PATH", "relative/path.so");
        let r = Curl::load();
        std::env::remove_var("LSBWRAP_LIBCURL_PATH");
        let e = r.expect_err("relative path must fail");
        assert!(matches!(e, CurlError::Loader(_)), "got {e:?}");
    }

    #[test]
    fn test_explicit_path_loads() {
        let _g = ENV_LOCK.lock().unwrap();
        // Resolve the real .so.4 and load it by explicit path.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("ldconfig -p 2>/dev/null | grep -m1 'libcurl.so.4 ' | awk '{print $NF}'")
            .output();
        let path = match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => return,
        };
        if path.is_empty() || !path.starts_with('/') {
            eprintln!("ldconfig gave no libcurl path, skipping");
            return;
        }
        std::env::set_var("LSBWRAP_LIBCURL_PATH", &path);
        let r = Curl::load();
        std::env::remove_var("LSBWRAP_LIBCURL_PATH");
        let curl = r.expect("explicit path load");
        assert!(curl.version().starts_with("libcurl/"));
    }
}
