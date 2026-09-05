//! Desktop window-title + clipboard inspection (Windows and Linux).
//!
//! `pir` runs as a process-per-terminal. When invoked from an *interactive*
//! terminal attached to the desktop session (the common case — you run `pir`
//! in Windows Terminal, or in an X11/Wayland terminal on Linux), the process
//! *can* see the titles of every top-level window and read the clipboard. The
//! data is purely a convenience for the `/login` helper, so every call is
//! best-effort: any failure returns an empty result rather than an error.
//!
//! On Windows we call the Win32 APIs directly (`EnumWindows` + `GetWindowTextW`
//! for titles; `OpenClipboard` / `GetClipboardData` for the clipboard), exactly
//! like the PowerShell `Get-Process | Where MainWindowTitle` / `Get-Clipboard`
//! cmdlets.
//!
//! On Linux we shell out to the standard desktop tools, because the underlying
//! APIs are fragmented between X11 and the various Wayland compositors:
//!   - window titles: `wmctrl -l` (X11; best-effort)
//!   - clipboard:     `xclip -selection clipboard -o` (X11) or
//!                    `wl-paste` (Wayland), whichever is present.
//! Missing tools simply yield empty results.

#![allow(dead_code)]

#[cfg(windows)]
pub mod impls {
    use std::ptr::null_mut;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, TRUE};
    // In windows-sys 0.61, the clipboard API moved from `UI::WindowsAndMessaging`
    // into `System::DataExchange`, and `CF_UNICODETEXT` lives there too.
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };

    // Guards clipboard access: `OpenClipboard` takes process-wide ownership of
    // the clipboard until `CloseClipboard`, so serialize to be safe.
    static CLIP_GUARD: Mutex<()> = Mutex::new(());

    /// A single visible top-level window's title + owning pid, mirroring the
    /// columns PowerShell's `Get-Process` shows for `MainWindowTitle`.
    pub struct WindowTitle {
        pub pid: u32,
        pub title: String,
    }

    /// Enumerate every visible top-level window with a non-empty title, in
    /// z-order (foreground-most first, which `EnumWindows` naturally yields).
    /// The current console window is skipped. Mirrors
    /// `Get-Process | Where-Object { $_.MainWindowTitle }`.
    pub fn window_titles() -> Vec<WindowTitle> {
        let mut out: Vec<WindowTitle> = Vec::new();
        unsafe {
            EnumWindows(Some(enum_cb), &mut out as *mut Vec<WindowTitle> as LPARAM);
        }
        out
    }

    // Free `extern "system" fn` trampoline passed to `EnumWindows` — required
    // because `EnumWindows` takes a raw fn pointer, not a closure. The result
    // vector is handed through the `LPARAM` (the call is single-threaded, from
    // the REPL thread).
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
        let out = &mut *(lparam as *mut Vec<WindowTitle>);
        if IsWindowVisible(hwnd) == 0 {
            return TRUE as i32;
        }
        let console = GetConsoleWindow();
        if hwnd == console {
            return TRUE as i32;
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if len <= 0 {
            return TRUE as i32;
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        if title.trim().is_empty() {
            return TRUE as i32;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        out.push(WindowTitle { pid, title });
        TRUE as i32
    }

    /// Read Unicode text from the system clipboard (Rust equivalent of
    /// `Get-Clipboard`). Returns an empty string when the clipboard is empty,
    /// holds non-text data, or is owned by another process. Best-effort.
    pub fn clipboard_text() -> String {
        let _lock = CLIP_GUARD.lock().unwrap();
        unsafe {
            if OpenClipboard(null_mut()) == 0 {
                return String::new();
            }
            // Close the clipboard on any return path (including early returns).
            let _guard = ScopeGuardClose;
            let h = GetClipboardData(CF_UNICODETEXT);
            if h.is_null() {
                return String::new();
            }
            // `GetClipboardData` returns memory owned by the clipboard; copy
            // out of it before closing. It is NUL-terminated UTF-16.
            let p = h as *const u16;
            if p.is_null() {
                return String::new();
            }
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
                if len > 1 << 20 {
                    break; // sanity cap (1 MiB)
                }
            }
            if len == 0 {
                return String::new();
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        }
    }

    /// RAII helper that closes the clipboard when dropped, even on early return.
    struct ScopeGuardClose;
    impl Drop for ScopeGuardClose {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
}

/// Linux implementation: uses `wmctrl -l` for window titles and `xclip`/`wl-paste`
/// for clipboard. Both are best-effort and degrade to empty results.
#[cfg(target_os = "linux")]
pub mod impls {
    use std::process::Command;

    pub struct WindowTitle {
        pub pid: u32,
        pub title: String,
    }

    /// Enumerate window titles using `wmctrl -l` (X11).
    ///
    /// `wmctrl -l` output looks like:
    /// ```text
    /// 0x01c00006  0 host-123  Title of the window
    /// ```
    /// where column 3 is the owning pid. On Wayland, or when `wmctrl` is not
    /// installed / there is no window manager, we simply return an empty list.
    pub fn window_titles() -> Vec<WindowTitle> {
        let Ok(output) = Command::new("wmctrl").arg("-l").output() else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut titles = Vec::new();
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Need at least: id, desktop, pid, and a non-empty title.
            if parts.len() < 4 {
                continue;
            }
            let Ok(pid) = parts[2].parse::<u32>() else {
                continue;
            };
            let title = parts[3..].join(" ");
            if title.trim().is_empty() {
                continue;
            }
            titles.push(WindowTitle { pid, title });
        }
        titles
    }

    /// Read text from the system clipboard.
    ///
    /// Tries `xclip` (X11) first, then `wl-paste` (Wayland). Returns an empty
    /// string when neither tool is available or the clipboard is empty /
    /// holds non-text data. Best-effort.
    pub fn clipboard_text() -> String {
        // X11: xclip -selection clipboard -o
        if let Ok(output) =
            Command::new("xclip").args(["-selection", "clipboard", "-o"]).output()
        {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
        }

        // Wayland: wl-paste
        if let Ok(output) = Command::new("wl-paste").output() {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
        }

        String::new()
    }
}

/// Fallback stub for non-Windows, non-Linux targets: always empty.
#[cfg(not(any(windows, target_os = "linux")))]
pub mod impls {
    pub struct WindowTitle {
        pub pid: u32,
        pub title: String,
    }
    pub fn window_titles() -> Vec<WindowTitle> {
        Vec::new()
    }
    pub fn clipboard_text() -> String {
        String::new()
    }
}

/// Guess the LLM provider from a candidate secret/key string. Returns the
/// catalog-style provider id (`openai`, `anthropic`, …) or `None` when the
/// format isn't recognised. Mirrors the heuristic shown to the user earlier:
///   - `sk-ant…`             → anthropic (classic key prefix)
///   - `AIza…`               → google
///   - `AI…`                 → anthropic (newer key prefix)
///   - `sk-proj-…`           → openai (project-scoped)
///   - `sk-…`                → openai (legacy user key)
///   - `xoxb-…`              → slack
///   - `ghp_` / `github_pat_`→ github
/// Anything else → `None`.
pub fn guess_provider_from_key(key: &str) -> Option<&'static str> {
    let k = key.trim();
    if k.is_empty() {
        return None;
    }
    // Order matters: the most specific prefixes win. `AIza…` is Google, not
    // Anthropic's `AI…` prefix, so it is tested before the `AI` anthropic rule.
    if k.starts_with("sk-ant") {
        Some("anthropic")
    } else if k.starts_with("AIza") {
        Some("google")
    } else if k.starts_with("AI") {
        Some("anthropic")
    } else if k.starts_with("sk-proj-") {
        Some("openai")
    } else if k.starts_with("sk-") {
        Some("openai")
    } else if k.starts_with("xoxb-") {
        Some("slack")
    } else if k.starts_with("ghp_") || k.starts_with("github_pat_") {
        Some("github")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_from_key_formats() {
        assert_eq!(guess_provider_from_key("sk-abc123"), Some("openai"));
        assert_eq!(guess_provider_from_key("sk-proj-abc123"), Some("openai"));
        assert_eq!(guess_provider_from_key("sk-ant-abc123"), Some("anthropic"));
        assert_eq!(guess_provider_from_key("AIzaabcdef"), Some("google"));
        assert_eq!(guess_provider_from_key("xoxb-123"), Some("slack"));
        assert_eq!(guess_provider_from_key("ghp_abcdef"), Some("github"));
        assert_eq!(guess_provider_from_key("github_pat_abc"), Some("github"));
        assert_eq!(guess_provider_from_key(""), None);
        assert_eq!(guess_provider_from_key("not-a-key"), None);
    }
}
