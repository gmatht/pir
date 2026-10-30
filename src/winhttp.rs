//! Native Windows HTTPS transport over WinHTTP (`winhttp.dll`).
//!
//! Why this exists: the `LsbCurl` backend dlopens the host `libcurl.so.4`,
//! which cannot exist on Windows, and the `CurlExe` fallback shells out to
//! the inbox `curl.exe`, which is absent (or blocked) on some machines. This
//! module talks to `winhttp.dll` directly — present on every Windows since
//! XP, TLS via Schannel against the machine certificate store, proxy from
//! the user's IE/WinHTTP configuration. No subprocess, no PATH lookup, no
//! third-party crate: the only new link is `windows-sys`'s
//! `Win32_Networking_WinHttp` feature, already a dependency.
//!
//! Two entry points, mirroring the `curl.exe` transport's contract:
//!
//! * [`request`] — buffered one-shot POST, returns `(status, body)`.
//! * [`stream_chunks`] — streaming POST: response bytes are pumped as
//!   `Vec<u8>` chunks over the caller's channel while the transfer runs, and
//!   the HTTP status is returned after EOF (WinHTTP, like libcurl, only
//!   reports it once the transfer completes).
//!
//! Timeouts mirror `lsb_curl::RequestOptions`: connect budget, total budget
//! (`WinHttpSetTimeouts` resolve/connect/send/receive), redirects followed by
//! default (WinHTTP does so unless disabled), peer verification on (Schannel
//! against the machine store; nothing disables it here).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Wide (UTF-16, nul-terminated) encoding of `s` for WinHTTP's `PCWSTR` args.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Last-Win32-error message for a failed WinHTTP call.
fn last_error(call: &str) -> String {
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("WinHTTP {call} failed (os error {code})")
}

/// Split `https://host[:port]/path...` into `(secure, host, port, path)`.
/// Only `http`/`https` are supported — the provider layer never issues
/// anything else, and anything else is a bug worth surfacing, not guessing.
fn split_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("WinHTTP: unsupported URL (no scheme): {url}"))?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(format!("WinHTTP: unsupported scheme in URL: {url}")),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => {
            let port: u16 = p
                .parse()
                .map_err(|_| format!("WinHTTP: bad port in URL: {url}"))?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("WinHTTP: empty host in URL: {url}"));
    }
    Ok((secure, host, port, path))
}

/// RAII wrapper for the three WinHTTP handles (session → connect → request),
/// closed in reverse order. `Send` so the streaming pump can own it on its
/// worker thread.
struct Session {
    session: *mut std::ffi::c_void,
    connect: *mut std::ffi::c_void,
    request: *mut std::ffi::c_void,
}

// SAFETY: handles are only touched from the owning thread (the streaming
// pump moves the whole `Session` to its worker); the raw pointers never alias.
unsafe impl Send for Session {}

impl Session {
    /// Open session → connect → request for a POST with `headers`. Returns
    /// the open request handle; the caller sends the body with
    /// [`Session::send`] and receives with [`Session::receive`].
    fn open(
        url: &str,
        headers: &[(String, String)],
        connect_timeout: Duration,
        total: Duration,
        agent: &str,
    ) -> Result<Self, String> {
        use windows_sys::Win32::Networking::WinHttp::*;
        let (secure, host, port, path) = split_url(url)?;
        let w_agent = wide(agent);
        let w_host = wide(&host);
        let w_path = wide(&path);
        // SAFETY: all WinHTTP calls below follow the documented open →
        // connect → request → send → receive sequence with nul-terminated
        // wide strings alive for the call duration; null handles are checked
        // before use and closed in `Drop`.
        unsafe {
            let session = WinHttpOpen(
                w_agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            if session.is_null() {
                return Err(last_error("WinHttpOpen"));
            }
            // Resolve/connect/send/receive budgets in ms. The total budget
            // maps onto send+receive (30s connect cap like the curl
            // transport; resolve fixed at 15s).
            let total_ms = total.as_millis().min(i32::MAX as u128) as i32;
            let connect_ms = connect_timeout.as_millis().min(30_000) as i32;
            if WinHttpSetTimeouts(session, 15_000, connect_ms, total_ms, total_ms) == 0 {
                let e = last_error("WinHttpSetTimeouts");
                WinHttpCloseHandle(session);
                return Err(e);
            }
            let connect = WinHttpConnect(session, w_host.as_ptr(), port, 0);
            if connect.is_null() {
                let e = last_error("WinHttpConnect");
                WinHttpCloseHandle(session);
                return Err(e);
            }
            let flags = if secure { WINHTTP_FLAG_SECURE } else { 0 };
            let request = WinHttpOpenRequest(
                connect,
                wide("POST").as_ptr(),
                w_path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                flags,
            );
            if request.is_null() {
                let e = last_error("WinHttpOpenRequest");
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err(e);
            }
            // One header block: "K: V\r\n..." plus content-type. A single
            // `WinHttpAddRequestHeaders` avoids per-header wide conversions.
            let mut block = String::new();
            for (k, v) in headers {
                block.push_str(k);
                block.push_str(": ");
                block.push_str(v);
                block.push_str("\r\n");
            }
            block.push_str("Content-Type: application/json\r\n");
            let w_block = wide(&block);
            if WinHttpAddRequestHeaders(
                request,
                w_block.as_ptr(),
                u32::MAX,
                WINHTTP_ADDREQ_FLAG_ADD,
            ) == 0
            {
                let e = last_error("WinHttpAddRequestHeaders");
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err(e);
            }
            // Stash the session/connect/request; the body goes out in `send`.
            Ok(Session { session, connect, request })
        }
    }

    /// Send the request with `body` and wait for the response headers.
    fn send(&self, body: &[u8]) -> Result<(), String> {
        use windows_sys::Win32::Networking::WinHttp::*;
        unsafe {
            let ok = WinHttpSendRequest(
                self.request,
                std::ptr::null(),
                0,
                if body.is_empty() { std::ptr::null() } else { body.as_ptr() as *const _ },
                body.len() as u32,
                body.len() as u32,
                0,
            );
            if ok == 0 {
                return Err(last_error("WinHttpSendRequest"));
            }
            if WinHttpReceiveResponse(self.request, std::ptr::null_mut()) == 0 {
                return Err(last_error("WinHttpReceiveResponse"));
            }
            Ok(())
        }
    }

    /// HTTP status code of the received response.
    fn status(&self) -> Result<u16, String> {
        use windows_sys::Win32::Networking::WinHttp::*;
        unsafe {
            let mut code: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            let ok = WinHttpQueryHeaders(
                self.request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut code as *mut _ as *mut _,
                &mut len,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return Err(last_error("WinHttpQueryHeaders(status)"));
            }
            Ok(code as u16)
        }
    }

    /// Read the whole response body into memory.
    fn read_all(&self) -> Result<Vec<u8>, String> {
        use windows_sys::Win32::Networking::WinHttp::*;
        let mut out = Vec::new();
        let mut buf = vec![0u8; 32 * 1024];
        unsafe {
            loop {
                let mut avail: u32 = 0;
                if WinHttpQueryDataAvailable(self.request, &mut avail) == 0 {
                    return Err(last_error("WinHttpQueryDataAvailable"));
                }
                if avail == 0 {
                    break;
                }
                let mut got: u32 = 0;
                let n = (avail as usize).min(buf.len()) as u32;
                if WinHttpReadData(
                    self.request,
                    buf.as_mut_ptr() as *mut _,
                    n,
                    &mut got,
                ) == 0
                {
                    return Err(last_error("WinHttpReadData"));
                }
                if got == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..got as usize]);
            }
        }
        Ok(out)
    }

    /// Pump response chunks until EOF. Returns `Err` on transport failure or
    /// when `cancel` flips (the caller maps that to "request cancelled").
    /// A `false` send (lost reader) ends the pump cleanly — the parser went
    /// away, so there is nothing left to feed.
    fn pump(&self, cancel: &AtomicBool, tx: &smol::channel::Sender<std::io::Result<Vec<u8>>>) -> Result<(), String> {
        use windows_sys::Win32::Networking::WinHttp::*;
        let mut buf = vec![0u8; 32 * 1024];
        unsafe {
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Err("request cancelled".to_string());
                }
                let mut avail: u32 = 0;
                if WinHttpQueryDataAvailable(self.request, &mut avail) == 0 {
                    return Err(last_error("WinHttpQueryDataAvailable"));
                }
                if avail == 0 {
                    return Ok(());
                }
                let mut got: u32 = 0;
                let n = (avail as usize).min(buf.len()) as u32;
                if WinHttpReadData(
                    self.request,
                    buf.as_mut_ptr() as *mut _,
                    n,
                    &mut got,
                ) == 0
                {
                    return Err(last_error("WinHttpReadData"));
                }
                if got == 0 {
                    return Ok(());
                }
                if tx.send_blocking(Ok(buf[..got as usize].to_vec())).is_err() {
                    return Ok(()); // parser went away; detach cleanly
                }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        use windows_sys::Win32::Networking::WinHttp::*;
        unsafe {
            // Request first, then connect, then session (reverse of creation).
            if !self.request.is_null() {
                WinHttpCloseHandle(self.request);
            }
            if !self.connect.is_null() {
                WinHttpCloseHandle(self.connect);
            }
            if !self.session.is_null() {
                WinHttpCloseHandle(self.session);
            }
        }
    }
}

/// Buffered one-shot POST through WinHTTP. Same contract as the curl.exe
/// buffered path: `(status, body)` on transport success, `Err` string on
/// failure. Only compiled on Windows; other platforms keep using libcurl.
pub fn request(
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    connect: Duration,
    total: Duration,
    agent: &str,
) -> Result<(u16, Vec<u8>), String> {
    let body = body.unwrap_or_default();
    let session = Session::open(url, headers, connect, total, agent)?;
    session.send(body)?;
    let status = session.status()?;
    let resp_body = session.read_all()?;
    Ok((status, resp_body))
}

/// Start a streaming WinHTTP POST on a dedicated thread and pump response
/// chunks into `tx` as they arrive (mirrors the curl.exe `stream_chunks`
/// contract: unbuffered, per-chunk cancel checks, status after EOF).
/// Returns the thread handle; the caller reads the status from `finish`.
pub fn stream_chunks(
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    cancel: Arc<AtomicBool>,
    connect: Duration,
    total: Duration,
    agent: String,
    tx: smol::channel::Sender<std::io::Result<Vec<u8>>>,
    finish: Arc<std::sync::Mutex<Option<String>>>,
    fail: impl Fn(u16, &str) -> Option<String> + Send + 'static,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("winhttp-stream".to_string())
        .spawn(move || {
            let terminal: Result<Option<String>, String> = (|| {
                let session = Session::open(&url, &headers, connect, total, &agent)?;
                session.send(&body)?;
                let status = session.status()?;
                if !(200..300).contains(&status) {
                    // Drain the (small) error body so the caller can report it.
                    let raw = session.read_all().unwrap_or_default();
                    return Ok(fail(status, &String::from_utf8_lossy(&raw)));
                }
                match session.pump(&cancel, &tx) {
                    Ok(()) => Ok(None),
                    Err(e) => Ok(Some(e)),
                }
            })();
            // Publish the terminal status *before* closing the channel so a
            // reader that observes EOF is guaranteed to see it.
            if let Ok(mut slot) = finish.lock() {
                *slot = match terminal {
                    Ok(msg) => msg,
                    Err(e) => Some(e),
                };
            }
            drop(tx); // clean EOF for the parsers
        })
        .map(|_| ())
        .map_err(|e| format!("winhttp stream thread: {e}"))
}

/// Synchronous buffered POST on the calling thread (used by tests only).
#[cfg(test)]
pub fn request_blocking(
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    agent: &str,
) -> Result<(u16, Vec<u8>), String> {
    request(url, headers, body, Duration::from_secs(15), Duration::from_secs(60), agent)
}
