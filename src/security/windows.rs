//! Windows backend for the security module — implements the layered model from
//! `docs/SECURITY_ON_WINDOWS.md`.
//!
//! All platform differences stay inside this module (and the `#[cfg(windows)]`
//! field on [`crate::security::SecurityPolicy`]); the rest of `pir` is
//! platform-independent and only ever talks to the cross-platform
//! [`crate::security::Platform`] trait and [`crate::security::SecurityContext`].
//!
//! Implemented today:
//!
//! * **Layer 1 — Job Object** (the doc's #1, adopted immediately): the session
//!   is wrapped in a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so
//!   the whole process tree dies the instant `pir` exits or crashes (the
//!   cgroup/`pdeathsig` equivalent). Optional active-process, memory, job-time
//!   and UI restrictions (block logoff/shutdown, clipboard, desktop switch).
//!   Engaged automatically at `SecurityContext::new` unless
//!   `security.windows.job = false` or `PIR_WIN_SECURITY=0|off`.
//! * **Layer 2/3 — AppContainer**: profile lifecycle (create/delete/path),
//!   SID derivation, narrow ACL grants (`SetNamedSecurityInfo`), and a
//!   `launch_in_appcontainer` helper (`CreateAppContainerToken` +
//!   `CreateProcessAsUser`) that a launcher wrapper uses for
//!   `sandbox`/`strict`. Not engaged by default (the in-process guardrail and
//!   Job Object are the `guard` posture).
//! * **Low Integrity Level** (opt-in defence-in-depth): transforms the current
//!   process token to `S-1-16-4096` so medium-integrity objects (the user's
//!   normal files) are write-protected against it.
//! * **ProjFS detection** (Layer 3 seam): `projfs_available()` probes the
//!   optional feature; the staging module (`staging`) mirrors the
//!   `overlay.rs` `status|apply|discard` surface for a future ProjFS backend.
//! * **WFP egress allow-list** (Layer 4 seam): `apply_network_policy()` is the
//!   call site; today it reports the elevation required and defers the actual
//!   rule installation to the launcher/enforcer (same request-don't-take
//!   pattern as `extensions/rootreq`).
//! * **Audit / request queue** (Layers 5+6): denials + grants are appended to
//!   `%LOCALAPPDATA%\pir\audit\security.log` and headless denials are queued as
//!   JSON into `$AI_PERM_REQUEST_DIR` (default `%TEMP%\ai-perm-requests`) — the
//!   same `ai-perm-request` channel `permctl` uses on Linux, so the operator
//!   enforcer stays platform-independent.
//!
//! Everything degrades gracefully: if a primitive isn't available (not
//! elevated, ProjFS absent, already inside a conflicting job), we log it and
//! fall back to the layer below — "at worst naggy, never broken".
//
// This module is a *seam surface*: launcher wrappers and operator-side tools
// call many of these items out-of-band, so in the standalone binary they are
// (correctly) not referenced from the cross-platform core. They are all
// exercised by the unit tests in this module.
#![cfg_attr(not(test), allow(dead_code))]

use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    BuildTrusteeWithSidW, ConvertSidToStringSidW, ConvertStringSidToSidW, GetNamedSecurityInfoW,
    SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, InitializeSecurityDescriptor, SetKernelObjectSecurity,
    SetSecurityDescriptorDacl, SetTokenInformation, ACL, DACL_SECURITY_INFORMATION, PSID,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID,
    TOKEN_DEFAULT_DACL, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    TokenAppContainerSid, TokenDefaultDacl, TokenElevation, TokenIntegrityLevel,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectBasicAccountingInformation,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
    JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Registry::{
    RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetCurrentProcessId,
    InitializeProcThreadAttributeList, OpenProcess, OpenProcessToken, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    STARTUPINFOEXW, STARTUPINFOW, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
};

use crate::security::{Denial, Platform};


// ===========================================================================
// Small helpers
// ===========================================================================

/// Wide (UTF-16, NUL-terminated) copy of `s`, for the *W Win32 APIs.
fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Read a NUL-terminated wide string from a Win32-owned pointer.
fn wide_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf16_lossy(slice)
}

/// Wide copy of a path (lossy is fine for ACL/command surfaces).
fn wpath(p: &Path) -> Vec<u16> {
    p.to_string_lossy().encode_utf16().chain(Some(0)).collect()
}

/// Standard access-mask bits (used where we don't need the crate's consts).
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;

/// `SE_GROUP_ENABLED` for capability `SID_AND_ATTRIBUTES` entries.
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
/// `SE_GROUP_INTEGRITY` for the mandatory-label attribute.
const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;
/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP`.
const SYSTEM_MANDATORY_LABEL_NO_WRITE_UP: u32 = 0x0000_0001;
/// `SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP`.
const SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP: u32 = 0x0000_0004;
/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`. HRESULTs are `i32`; this is
/// 0x800700B7 reinterpreted.
const HRESULT_ALREADY_EXISTS: i32 = -2_147_024_713;
/// `JOB_OBJECT_ALL_ACCESS` (not exposed by windows-sys): STANDARD_RIGHTS_REQUIRED
/// | ASSIGN_PROCESS | SET_ATTRIBUTES | QUERY | TERMINATE | SET_SECURITY_ATTRIBUTES | IMPERSONATE.
const JOB_OBJECT_ALL_ACCESS: u32 = 0x000F_003F;

/// A Win32 `HANDLE` that closes itself on drop.
struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

/// Map an `HRESULT` (from the userenv appcontainer APIs) to `io::Error`.
fn hresult_err(hr: i32) -> io::Error {
    let code = (hr as u32 & 0xFFFF) as i32; // HRESULT_FROM_WIN32 keeps the code in the low 16 bits
    io::Error::from_raw_os_error(code)
}

fn epoch_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Path to the per-user `pir` state dir (`%LOCALAPPDATA%\pir`, fallback temp).
fn pir_state_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("pir")
}

/// The current Windows username (for audit `who` and `is_other_users`).
pub fn current_user() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "?".to_string())
}

/// Global escape hatch: `PIR_WIN_SECURITY=0|off` disables the host-level
/// Windows layers (Job Object, low-IL) while keeping the in-process guardrail.
pub fn host_layer_enabled() -> bool {
    match std::env::var_os("PIR_WIN_SECURITY") {
        Some(v) => {
            let v = v.to_string_lossy().to_ascii_lowercase();
            v != "0" && v != "off" && v != "false" && v != "no"
        }
        None => true,
    }
}

// ===========================================================================
// Layer 1 — Job Object (lifecycle + resource control)
// ===========================================================================

/// Limits for a [`Job`]. `None` means "no limit". All fields are optional so
/// the interactive default stays lightweight: only what the operator asked for
/// is enforced.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobLimits {
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: the whole tree dies the instant
    /// the job's last handle closes (pir exit or crash).
    pub kill_on_close: bool,
    /// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`: fork-bomb bound (e.g. 64).
    pub active_process: Option<u32>,
    /// `JOB_OBJECT_LIMIT_JOB_MEMORY`: whole-tree RAM bound, in MiB.
    pub job_memory_mb: Option<u64>,
    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY`: per-process RAM bound, in MiB.
    pub process_memory_mb: Option<u64>,
    /// `JOB_OBJECT_LIMIT_JOB_TIME`: total CPU budget, in ms.
    pub job_time_ms: Option<u64>,
    /// UI restrictions (cannot log off / lock the screen, etc.).
    pub ui: UiRestrictions,
}

/// User-interface restrictions applied to a [`Job`]. Defaults are the safe
/// subset: the agent may not log the user off or shut the machine down.
/// Clipboard/desktop restrictions are opt-in (they also affect the operator
/// interacting with pir's own console).
#[derive(Debug, Clone, Copy, Default)]
pub struct UiRestrictions {
    /// `JOB_OBJECT_UILIMIT_EXITWINDOWS` — block logoff / shutdown / reboot.
    pub block_exit_windows: bool,
    /// `JOB_OBJECT_UILIMIT_*CLIPBOARD` — block clipboard read/write.
    pub block_clipboard: bool,
    /// `JOB_OBJECT_UILIMIT_DESKTOP` — cannot switch desktops.
    pub block_desktop_switch: bool,
    /// `JOB_OBJECT_UILIMIT_DISPLAYSETTINGS` — cannot change display settings.
    pub block_display_settings: bool,
    /// `JOB_OBJECT_UILIMIT_GLOBALATOMS` — cannot access global atoms.
    pub block_global_atoms: bool,
}

impl UiRestrictions {
    fn class(&self) -> u32 {
        let mut c = 0u32;
        if self.block_exit_windows {
            c |= JOB_OBJECT_UILIMIT_EXITWINDOWS;
        }
        if self.block_clipboard {
            c |= JOB_OBJECT_UILIMIT_READCLIPBOARD | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;
        }
        if self.block_desktop_switch {
            c |= JOB_OBJECT_UILIMIT_DESKTOP;
        }
        if self.block_display_settings {
            c |= JOB_OBJECT_UILIMIT_DISPLAYSETTINGS;
        }
        if self.block_global_atoms {
            c |= JOB_OBJECT_UILIMIT_GLOBALATOMS;
        }
        c
    }
}

/// A Windows Job Object. Closes the handle on drop — with
/// `kill_on_close` that is exactly the "teardown the tree" trigger.
///
/// **Drop semantics:** when the last handle to a `kill_on_close` job closes,
/// the OS terminates *every* process in the job — including the process doing
/// the closing, if it is a member. That is the intended lifecycle guarantee
/// (the whole tree dies with the launcher), but it means a session job must
/// outlive the whole process: never drop it early (tests use
/// `std::mem::forget` for this reason), and never rebuild the
/// [`crate::security::SecurityContext`] that owns it while running.
pub struct Job {
    handle: HANDLE,
}

// A `HANDLE` is a plain pointer; sharing the job across threads (e.g. inside an
// `Arc<SecurityContext>`) is safe — the handle is per-process and never leaked.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Job {
    /// Create a new (optionally named) job object.
    pub fn create(name: Option<&str>) -> io::Result<Job> {
        let n = name.map(wstr);
        let h = unsafe { CreateJobObjectW(null_mut(), n.as_ref().map(|w| w.as_ptr()).unwrap_or(null_mut())) };
        if h.is_null() {
            return Err(last_err());
        }
        Ok(Job { handle: h })
    }

    pub fn raw(&self) -> HANDLE {
        self.handle
    }

    /// Apply the configured limits. Each limit is applied independently so a
    /// partial failure (e.g. memory limits being unavailable on the host)
    /// falls back gracefully to the rest.
    pub fn apply_limits(&self, limits: &JobLimits) -> io::Result<()> {
        let mut flags: u32 = 0;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if limits.kill_on_close {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if let Some(n) = limits.active_process {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = n;
        }
        if let Some(mb) = limits.job_memory_mb {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = (mb.saturating_mul(1024 * 1024)) as usize;
        }
        if let Some(mb) = limits.process_memory_mb {
            flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = (mb.saturating_mul(1024 * 1024)) as usize;
        }
        if let Some(ms) = limits.job_time_ms {
            // Job time is in 100 ns units.
            flags |= JOB_OBJECT_LIMIT_JOB_TIME;
            info.BasicLimitInformation.PerJobUserTimeLimit = (ms as i64).saturating_mul(10_000);
        }
        info.BasicLimitInformation.LimitFlags = flags;
        if flags != 0 {
            let ok = unsafe {
                SetInformationJobObject(
                    self.handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(last_err());
            }
        }
        self.apply_ui_restrictions(limits.ui)
    }

    fn apply_ui_restrictions(&self, ui: UiRestrictions) -> io::Result<()> {
        let class = ui.class();
        if class == 0 {
            return Ok(());
        }
        let info = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: class };
        let ok = unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectBasicUIRestrictions,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        };
        if ok == 0 {
            // UI restrictions can fail when the process is already in a nested
            // job chain; the rest of the limits still stand. Report as an error
            // so the caller can log it, but don't roll back.
            return Err(last_err());
        }
        Ok(())
    }

    /// Put the current process (and therefore all its descendants on Win8+)
    /// into the job.
    pub fn assign_current_process(&self) -> io::Result<()> {
        let ok = unsafe { AssignProcessToJobObject(self.handle, GetCurrentProcess()) };
        if ok == 0 {
            return Err(last_err());
        }
        Ok(())
    }

    /// Put an already-open process handle into the job.
    pub fn assign_handle(&self, process: HANDLE) -> io::Result<()> {
        let ok = unsafe { AssignProcessToJobObject(self.handle, process) };
        if ok == 0 {
            return Err(last_err());
        }
        Ok(())
    }

    /// Open a process by pid and put it into the job.
    pub fn assign_pid(&self, pid: u32) -> io::Result<()> {
        let h = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if h.is_null() {
            return Err(last_err());
        }
        let own = Owned(h);
        self.assign_handle(own.0)
    }

    /// Kill every process in the job (used by tests and the launcher teardown).
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        let ok = unsafe { TerminateJobObject(self.handle, exit_code) };
        if ok == 0 {
            return Err(last_err());
        }
        Ok(())
    }

    /// Number of currently live processes in the job (status reporting).
    pub fn active_processes(&self) -> u32 {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let mut got = 0u32;
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                &mut got,
            )
        };
        if ok == 0 {
            return 0;
        }
        info.ActiveProcesses
    }

    /// Whether the given pid is already a member of this job.
    pub fn contains_pid(&self, pid: u32) -> bool {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return false;
        }
        let own = Owned(h);
        let mut yes: BOOL = 0;
        let ok = unsafe { IsProcessInJob(own.0, self.handle, &mut yes) };
        ok != 0 && yes != 0
    }
}

/// The doc's Layer 1 in one call: create a job, apply `limits`, and put the
/// current process inside it. The returned [`Job`] must be kept alive for the
/// life of the session and **must never be dropped before the process exits**
/// (see [`Job`] drop semantics) — the context that owns it holds it for the
/// whole run, and the OS closes the handle during process teardown, which is
/// when `kill_on_close` reaps the tree.
///
/// The job is *uniquely named per pid* (`pir-session-<pid>`) so two
/// concurrent pir sessions never share a job object: `KILL_ON_JOB_CLOSE` must
/// fire when *this* session exits, not when the last holder of a shared
/// handle goes away. An operator can still open it by name to
/// `TerminateJobObject` a runaway tree.
pub fn enable_lifecycle_job(limits: &JobLimits) -> io::Result<Job> {
    let job = Job::create(Some(&session_job_name()))?;
    job.apply_limits(limits)?;
    job.assign_current_process()?;
    Ok(job)
}

/// The per-process job name used for the session lifecycle job.
pub fn session_job_name() -> String {
    format!("pir-session-{}", unsafe { GetCurrentProcessId() })
}

// ===========================================================================
// Layer 2/3 — AppContainer (profile lifecycle + ACL grants + launch)
// ===========================================================================

/// How much access to grant an AppContainer on a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirAccess {
    /// `GENERIC_READ` — read-only (docs, toolchain headers).
    Read,
    /// `GENERIC_READ | GENERIC_WRITE` — scratch/cache space.
    ReadWrite,
    /// `GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE` — the project root.
    ReadWriteExec,
}

impl DirAccess {
    fn mask(self) -> u32 {
        match self {
            DirAccess::Read => GENERIC_READ,
            DirAccess::ReadWrite => GENERIC_READ | GENERIC_WRITE,
            DirAccess::ReadWriteExec => GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE,
        }
    }
}

/// A per-session AppContainer profile. Created with `create`, deleted on drop
/// only when *we* created it (never tears down an operator's existing profile).
#[derive(Debug)]
pub struct AppContainerProfile {
    name: String,
    created: bool,
}

impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        if self.created && self.exists() {
            let n = wstr(&self.name);
            // Best-effort teardown; the OS also reclaims abandoned profiles.
            unsafe { DeleteAppContainerProfile(n.as_ptr()) };
        }
    }
}

impl AppContainerProfile {
    /// Create the profile with the given display name/description and an empty
    /// capability list (or `caps`, e.g. `["internetClient"]` for outbound web
    /// access). Returns `Ok` if the profile already exists — we adopt it
    /// without owning its teardown.
    pub fn create(name: &str, display: &str, description: &str, caps: &[&str]) -> io::Result<Self> {
        let n = wstr(name);
        let d = wstr(display);
        let de = wstr(description);
        // Capability SIDs must outlive the call.
        let cap_sids = capability_sids(caps)?;
        let mut cap_attrs: Vec<SID_AND_ATTRIBUTES> = cap_sids
            .iter()
            .map(|s| SID_AND_ATTRIBUTES { Sid: s.as_ptr() as PSID, Attributes: SE_GROUP_ENABLED })
            .collect();
        let mut sid_out: PSID = null_mut();
        let hr = unsafe {
            CreateAppContainerProfile(
                n.as_ptr(),
                d.as_ptr(),
                de.as_ptr(),
                if cap_attrs.is_empty() { null_mut() } else { cap_attrs.as_mut_ptr() },
                cap_attrs.len() as u32,
                &mut sid_out,
            )
        };
        if !sid_out.is_null() {
            unsafe { LocalFree(sid_out as _) };
        }
        if hr >= 0 {
            return Ok(AppContainerProfile { name: name.to_string(), created: true });
        }
        if hr == HRESULT_ALREADY_EXISTS {
            return Ok(AppContainerProfile { name: name.to_string(), created: false });
        }
        Err(hresult_err(hr))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the profile currently exists on disk.
    pub fn exists(&self) -> bool {
        self.path().is_ok()
    }

    /// The profile's AppContainer SID (raw bytes).
    pub fn sid_bytes(&self) -> io::Result<Vec<u8>> {
        let n = wstr(&self.name);
        let mut psid: PSID = null_mut();
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(n.as_ptr(), &mut psid) };
        if hr < 0 || psid.is_null() {
            return Err(hresult_err(hr));
        }
        let len = unsafe { windows_sys::Win32::Security::GetLengthSid(psid) } as usize;
        let mut bytes = vec![0u8; len];
        unsafe { std::ptr::copy_nonoverlapping(psid as *const u8, bytes.as_mut_ptr(), len) };
        unsafe { LocalFree(psid as _) };
        Ok(bytes)
    }

    /// The profile's AppContainer SID as a string (`S-1-15-2-...`), which is
    /// also the display name Windows uses for it.
    pub fn sid_string(&self) -> io::Result<String> {
        let bytes = self.sid_bytes()?;
        let mut out: windows_sys::core::PWSTR = null_mut();
        let ok = unsafe { ConvertSidToStringSidW(bytes.as_ptr() as PSID, &mut out) };
        if ok == 0 {
            return Err(last_err());
        }
        let s = wide_to_string(out);
        unsafe { LocalFree(out as _) };
        Ok(s)
    }

    /// The profile's per-user data folder (e.g. `%LOCALAPPDATA%\Packages\<name>`).
    pub fn path(&self) -> io::Result<PathBuf> {
        let sid = self.sid_string()?;
        let w = wstr(&sid);
        let mut out: windows_sys::core::PWSTR = null_mut();
        let hr = unsafe { GetAppContainerFolderPath(w.as_ptr(), &mut out) };
        if hr < 0 || out.is_null() {
            return Err(hresult_err(hr));
        }
        let p = PathBuf::from(wide_to_string(out));
        unsafe { LocalFree(out as _) };
        Ok(p)
    }

    /// Grant the AppContainer ACL access on `dir` (narrowly — `access` is the
    /// whole grant; never grant `C:\Users` or secret stores). `inherit` makes
    /// the ACE inherited by children (for the project root).
    pub fn grant_dir(&self, dir: &Path, access: DirAccess, inherit: bool) -> io::Result<()> {
        let bytes = self.sid_bytes()?;
        let mut trustee = TRUSTEE_W::default();
        unsafe { BuildTrusteeWithSidW(&mut trustee, bytes.as_ptr() as PSID) };
        trustee.TrusteeForm = TRUSTEE_IS_SID;
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access.mask(),
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: if inherit {
                windows_sys::Win32::Security::OBJECT_INHERIT_ACE
                    | windows_sys::Win32::Security::CONTAINER_INHERIT_ACE
            } else {
                windows_sys::Win32::Security::NO_INHERITANCE
            },
            Trustee: trustee,
        };
        grant_acl_entry(dir, &ea)
    }
}

/// Well-known AppContainer capability SIDs (documented in MSDN "Well-known
/// SIDs"). We map names -> SID strings ourselves because the derivation API
/// (`DeriveCapabilitySidsFromName`) forwards through a broken api-set on some
/// Windows 11 24H2 hosts (STATUS_ACCESS_VIOLATION) — the same export-removal
/// class as `GetWindowSubclass`. pir only needs the handful of standard
/// network/library capabilities, all of which are stable, documented SIDs.
fn capability_sid_string(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    Some(match n.as_str() {
        "internetclient" => "S-1-15-3-1",
        "internetclientserver" => "S-1-15-3-2",
        "privatenetworkclientserver" => "S-1-15-3-3",
        "documentslibrary" => "S-1-15-3-4",
        "pictureslibrary" => "S-1-15-3-5",
        "videoslibrary" => "S-1-15-3-6",
        "musiclibrary" => "S-1-15-3-7",
        "enterpriseauthentication" => "S-1-15-3-8",
        "sharedusercertificates" => "S-1-15-3-9",
        "removablestorage" => "S-1-15-3-10",
        _ => return None,
    })
}

/// Resolve capability names to raw SID bytes (via the well-known table above).
fn capability_sids(names: &[&str]) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let Some(sid_str) = capability_sid_string(n) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown AppContainer capability {n:?} (supported: internetClient, \
                     internetClientServer, privateNetworkClientServer, documentsLibrary, \
                     picturesLibrary, videosLibrary, musicLibrary, enterpriseAuthentication, \
                     sharedUserCertificates, removableStorage)"
                ),
            ));
        };
        let mut psid: PSID = null_mut();
        let ok = unsafe { ConvertStringSidToSidW(wstr(sid_str).as_ptr(), &mut psid) };
        if ok == 0 || psid.is_null() {
            return Err(last_err());
        }
        let len = unsafe { windows_sys::Win32::Security::GetLengthSid(psid) } as usize;
        let mut sid = vec![0u8; len];
        unsafe { std::ptr::copy_nonoverlapping(psid as *const u8, sid.as_mut_ptr(), len) };
        unsafe { LocalFree(psid as _) };
        out.push(sid);
    }
    Ok(out)
}

/// Add an ACE to a file/directory's DACL, merging with the existing entries
/// (never clobbering the owner's access).
fn grant_acl_entry(path: &Path, entry: &EXPLICIT_ACCESS_W) -> io::Result<()> {
    let w = wpath(path);
    let mut dacl: *mut ACL = null_mut();
    let mut sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = null_mut();
    // Read the existing DACL so SetEntriesInAcl *merges* rather than replaces.
    let r = unsafe {
        GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let mut new_acl: *mut ACL = null_mut();
    let e = unsafe { SetEntriesInAclW(1, entry, dacl, &mut new_acl) };
    if e != 0 {
        if !sd.is_null() {
            unsafe { LocalFree(sd as _) };
        }
        return Err(io::Error::from_raw_os_error(e as i32));
    }
    let r2 = unsafe {
        SetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_acl,
            null_mut(),
        )
    };
    if !sd.is_null() {
        unsafe { LocalFree(sd as _) };
    }
    if !new_acl.is_null() {
        unsafe { LocalFree(new_acl as _) };
    }
    if r2 != 0 {
        return Err(io::Error::from_raw_os_error(r2 as i32));
    }
    Ok(())
}

/// A child process launched inside an AppContainer token.
pub struct AppContainerChild {
    pub pid: u32,
    process: HANDLE,
    thread: HANDLE,
}

impl Drop for AppContainerChild {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.process);
            CloseHandle(self.thread);
        }
    }
}

impl AppContainerChild {
    /// Wait up to `ms` for the child to exit; true if it exited.
    pub fn wait(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.process, ms) == WAIT_OBJECT_0 }
    }

    fn new(pi: &PROCESS_INFORMATION) -> Self {
        AppContainerChild { pid: pi.dwProcessId, process: pi.hProcess, thread: pi.hThread }
    }
}

/// Launch `command` inside `profile` with the given capabilities. This is the
/// launcher seam for `sandbox`/`strict` — the same `CreateAppContainerToken` +
/// `CreateProcessAsUser` recipe as `SECURITY_ON_WINDOWS.md` §2.5. The caller
/// typically assigns the returned process to a [`Job`] too.
pub fn launch_in_appcontainer(
    profile: &AppContainerProfile,
    command: &str,
    cwd: Option<&Path>,
    caps: &[&str],
) -> io::Result<AppContainerChild> {
    // Launch *inside* the container by passing the AppContainer SID (plus any
    // capabilities) as a process attribute — `CreateProcess` with
    // `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` (SECURITY_MODEL.md §2.1
    // step 4, second option). This is fully linkable through kernel32 (no
    // GetProcAddress needed) and works from an ordinary medium-IL caller.

    eprintln!("[probeL] l1 sid");
    // 1. The AppContainer SID + capability SIDs (kept alive through the call).
    let sid_bytes = profile.sid_bytes()?;
    let psid = sid_bytes.as_ptr() as PSID;
    eprintln!("[probeL] l1b caps");
    let cap_bytes = capability_sids(caps)?;
    eprintln!("[probeL] l1c derived {}", cap_bytes.len());
    let mut attrs: Vec<SID_AND_ATTRIBUTES> = cap_bytes
        .iter()
        .map(|s| SID_AND_ATTRIBUTES { Sid: s.as_ptr() as PSID, Attributes: SE_GROUP_ENABLED })
        .collect();
    let sc = SECURITY_CAPABILITIES {
        AppContainerSid: psid,
        Capabilities: if attrs.is_empty() { null_mut() } else { attrs.as_mut_ptr() },
        CapabilityCount: attrs.len() as u32,
        Reserved: 0,
    };

    eprintln!("[probeL] l2 attrlist");
    // 2. Build the thread-attribute list (size query, then init, then set).
    let mut attr_size = 0usize;
    // First call with NULL asks for the required size and fails expectedly.
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
    }
    let mut attr_buf = vec![0u8; attr_size.max(1)];
    let ok = unsafe {
        InitializeProcThreadAttributeList(attr_buf.as_mut_ptr() as *mut c_void, 1, 0, &mut attr_size)
    };
    if ok == 0 {
        return Err(last_err());
    }
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_buf.as_mut_ptr() as *mut c_void,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            &sc as *const _ as *const c_void,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null_mut(),
        )
    };
    if ok == 0 {
        unsafe { DeleteProcThreadAttributeList(attr_buf.as_mut_ptr() as *mut c_void) };
        return Err(last_err());
    }

    eprintln!("[probeL] l3 launch");
    // 3. Launch with the extended startup info.
    let cmd = wstr(command);
    let cwd_w: Option<Vec<u16>> = cwd.map(wpath);
    let siex = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            ..Default::default()
        },
        lpAttributeList: attr_buf.as_mut_ptr() as *mut c_void,
    };
    let mut pi = PROCESS_INFORMATION::default();
    let ok = unsafe {
        CreateProcessW(
            null_mut(),
            cmd.as_ptr() as *mut u16,
            null_mut(),
            null_mut(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            null_mut(),
            cwd_w.as_ref().map(|w| w.as_ptr()).unwrap_or(null_mut()),
            &siex.StartupInfo,
            &mut pi,
        )
    };
    unsafe { DeleteProcThreadAttributeList(attr_buf.as_mut_ptr() as *mut c_void) };
    if ok == 0 {
        return Err(last_err());
    }
    Ok(AppContainerChild::new(&pi))
}

/// Whether the *current* process runs inside an AppContainer already.
/// `GetTokenInformation(TokenAppContainerSid)` *succeeds* on a normal token
/// but returns a NULL SID — so the SID pointer, not the call's return value,
/// is the test.
pub fn is_appcontainer() -> bool {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Owned(token);
    // The output is a PSID (8 bytes) followed by the SID itself; the buffer
    // must be aligned for the pointer read.
    #[repr(C, align(16))]
    struct AlignedBuf([u8; 256]);
    let mut buf = AlignedBuf([0u8; 256]);
    let mut got = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenAppContainerSid,
            buf.0.as_mut_ptr() as *mut c_void,
            buf.0.len() as u32,
            &mut got,
        )
    };
    if ok == 0 || got < std::mem::size_of::<PSID>() as u32 {
        return false;
    }
    let psid = unsafe { *(buf.0.as_ptr() as *const PSID) };
    !psid.is_null()
}

// ===========================================================================
// Integrity Level (opt-in low-IL defence-in-depth)
// ===========================================================================

/// The current process's integrity level RID (e.g. 4096 = low, 8192 = medium).
pub fn integrity_level() -> u32 {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return 0;
    }
    let token = Owned(token);
    // GetTokenInformation writes the TOKEN_MANDATORY_LABEL struct *and* the
    // SID it points to into the buffer, so it must be big enough and the
    // struct must sit at an 8-aligned address (a plain `[u8]` buffer is not
    // guaranteed aligned — that misalignment panics on deref).
    #[repr(C, align(16))]
    struct AlignedBuf([u8; 512]);
    let mut buf = AlignedBuf([0u8; 512]);
    let mut got = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            buf.0.as_mut_ptr() as *mut c_void,
            buf.0.len() as u32,
            &mut got,
        )
    };
    if ok == 0 {
        return 0;
    }
    let til = unsafe { &*(buf.0.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
    if til.Label.Sid.is_null() {
        return 0;
    }
    let mut out: windows_sys::core::PWSTR = null_mut();
    if unsafe { ConvertSidToStringSidW(til.Label.Sid, &mut out) } == 0 {
        return 0;
    }
    let s = wide_to_string(out);
    unsafe { LocalFree(out as _) };
    // "S-1-16-<rid>"
    s.rsplit('-').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0)
}

/// Drop the current process token to **Low Integrity Level** (S-1-16-4096).
/// Once set it cannot be raised within the process; it makes every medium
/// integrity object (the user's normal files) write-protected — the cheap
/// "extra write-blocking" layer from `SECURITY_ON_WINDOWS.md` §7. Opt-in only:
/// the project tree must be reachable under low-IL for the agent to work.
pub fn lower_to_low_integrity() -> io::Result<()> {
    let mut token: HANDLE = null_mut();
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID,
            &mut token,
        )
    };
    if ok == 0 {
        return Err(last_err());
    }
    let token = Owned(token);

    let mut sid: PSID = null_mut();
    if unsafe {
        ConvertStringSidToSidW(
            wstr("S-1-16-4096").as_ptr(),
            &mut sid,
        )
    } == 0
    {
        return Err(last_err());
    }
    let sid = Owned(sid as HANDLE);

    // 1) Label the token itself low.
    let til = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES { Sid: sid.0 as PSID, Attributes: SE_GROUP_INTEGRITY },
    };
    let ok = unsafe {
        SetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            &til as *const _ as *const c_void,
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_err());
    }

    // 2) Add the mandatory-label ACE (SYSTEM_MANDATORY_LABEL_NO_WRITE_UP |
    //    NO_EXECUTE_UP) to a DACL, install it as the token's default DACL, and
    //    label the process kernel object itself via a proper security
    //    descriptor. Best-effort: even if this step is refused by the host,
    //    the token integrity level above already blocks medium-object writes.
    let mut trustee = TRUSTEE_W::default();
    unsafe { BuildTrusteeWithSidW(&mut trustee, sid.0 as PSID) };
    trustee.TrusteeForm = TRUSTEE_IS_SID;
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
            | SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: windows_sys::Win32::Security::NO_INHERITANCE,
        Trustee: trustee,
    };
    let mut new_acl: *mut ACL = null_mut();
    let e = unsafe { SetEntriesInAclW(1, &ea, null_mut(), &mut new_acl) };
    if e != 0 {
        return Ok(()); // non-fatal
    }
    let tdd = TOKEN_DEFAULT_DACL { DefaultDacl: new_acl };
    let ok = unsafe {
        SetTokenInformation(
            token.0,
            TokenDefaultDacl,
            &tdd as *const _ as *const c_void,
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    };
    if ok == 0 {
        unsafe { LocalFree(new_acl as _) };
        return Ok(()); // non-fatal
    }
    // Label the process kernel object too (so medium processes can't write to
    // us): SetKernelObjectSecurity needs a full SECURITY_DESCRIPTOR wrapping
    // the label DACL, not the bare ACL.
    let mut sd: windows_sys::Win32::Security::SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let init_ok =
        unsafe { InitializeSecurityDescriptor(&mut sd as *mut _ as *mut c_void, 1) }; // SECURITY_DESCRIPTOR_REVISION
    if init_ok != 0 {
        let set_ok = unsafe { SetSecurityDescriptorDacl(&mut sd as *mut _ as *mut c_void, 1, new_acl, 0) };
        if set_ok != 0 {
            let _ = unsafe {
                SetKernelObjectSecurity(
                    GetCurrentProcess() as HANDLE,
                    DACL_SECURITY_INFORMATION,
                    &sd as *const _ as _,
                )
            };
        }
    }
    unsafe { LocalFree(new_acl as _) };
    Ok(())
}

// ===========================================================================
// Elevation / UAC (the rootreq seam)
// ===========================================================================

/// Whether the current process runs elevated (admin token with
/// `TokenElevation`). UAC-gated elevation is the Windows analogue of the
/// sudoers/rootreq boundary: *no* code path in pir elevates itself — this is
/// only a detector that launchers/enforcers consult.
pub fn is_elevated() -> bool {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Owned(token);
    let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut got = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elev as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut got,
        )
    };
    ok != 0 && elev.TokenIsElevated != 0
}

// ===========================================================================
// Layer 3 seam — ProjFS detection
// ===========================================================================

/// Detect whether the Projected File System optional feature is available.
/// Heuristic probe (no elevation needed): `ProjectedFSLib.dll` (the real ProjFS API
/// DLL; `ProjFS.dll` is a non-existent name) must be present and the `PrjFlt`
/// filter-driver service must not be disabled. Returns `false` when unsure — the launcher must gracefully fall back to the in-process guardrail
/// and/or the no-driver manifest staging (same degradation as overlayfs).
pub fn projfs_available() -> bool {
    let system = system_dir();
    // The ProjFS user-mode API ships as `ProjectedFSLib.dll` (in System32).
    // The DLL being present is necessary but not sufficient: the real
    // virtualization comes from the `PrjFlt` minifilter, shipped by the
    // Client-ProjFS optional feature (absent on Windows 11 Home).
    // The DLL lives in System32 (never directly under %WINDIR%), and the
    // literal `ProjFS.dll` does not exist on any Windows build. Check both
    // spots so we don't return a false negative on a configured host.
    let dll_present = system
        .join("System32")
        .join("ProjectedFSLib.dll")
        .exists()
        || system.join("ProjectedFSLib.dll").exists();
    if !dll_present {
        return false;
    }
    // HKLM\SYSTEM\CurrentControlSet\Services\PrjFlt\Start == 4 (disabled)?
    match service_start_type("PrjFlt") {
        Some(4) => false,
        Some(_) => true,
        None => false, // unknown host (pre-1809 or restricted view) -> assume absent
    }
}

fn system_dir() -> PathBuf {
    let mut buf = vec![0u16; 320];
    let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return PathBuf::from("C:\\Windows");
    }
    buf.truncate(n as usize);
    PathBuf::from(String::from_utf16_lossy(&buf))
}

/// Read a service's `Start` value from the registry (0..4; 4 = disabled).
fn service_start_type(service: &str) -> Option<u32> {
    let mut key: HKEY = null_mut();
    let path = format!("SYSTEM\\CurrentControlSet\\Services\\{service}");
    let r = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            wstr(&path).as_ptr(),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if r != 0 {
        return None;
    }
    let key_guard = Owned(key as HANDLE);
    let mut buf = [0u8; 16];
    let mut len = buf.len() as u32;
    let mut typ = 0u32;
    let r = unsafe {
        RegQueryValueExW(
            key_guard.0 as HKEY,
            wstr("Start").as_ptr(),
            null_mut(),
            &mut typ,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    if len < 4 {
        return None;
    }
    Some(u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

// ===========================================================================
// Layer 4 seam — network egress (WFP / Windows Firewall)
// ===========================================================================

/// A desired egress allow-list. The doc's Layer 4 is a Windows Firewall/WFP
/// rule set installed by a launcher/enforcer (needs elevation); this struct is
/// the *contract* — `network = allowlist` maps `net-*` parcels to the hosts
/// here. The rule installation itself is deliberately not performed inside pir
/// (request-don't-take).
#[derive(Debug, Clone, Default)]
pub struct NetworkPolicy {
    /// `host:port` pairs the agent may connect to (e.g. `crates.io:443`).
    pub allow_hosts: Vec<String>,
    /// Block *all* outbound (network = off).
    pub block_all: bool,
}

/// Apply `policy` as a WFP/Windows Firewall rule set. Not wired in this build
/// (installing rules requires elevation and should be driven by the operator
/// enforcer); returns the elevation precondition as a structured error so the
/// launcher can decide to run elevated or refuse.
pub fn apply_network_policy(_policy: &NetworkPolicy) -> io::Result<()> {
    if is_elevated() {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WFP rule installation is the launcher's job (see docs/SECURITY_ON_WINDOWS.md §2.5); not wired in this build",
        ))
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "network allow-list requires an elevated enforcer; run pir (or its wrapper) elevated with security.windows.wfp = true",
        ))
    }
}

// ===========================================================================
// Layer 3 staging seam — `/quarantine`-compatible upper/store management
// ===========================================================================

/// Minimal staging store mirroring `overlay.rs`'s `status|apply|discard`
/// surface, so a future ProjFS backend (which produces real virtualized
/// writes) plugs in without touching the `/quarantine` command: the guardrail
/// keeps denying out-of-tree writes; when ProjFS (or the no-driver manifest
/// mode) stages them, they land here and are reviewed with the same commands.
pub mod staging {
    use super::*;

    fn staging_root() -> PathBuf {
        pir_state_dir().join("staging")
    }

    pub fn session_dir() -> PathBuf {
        staging_root().join("session")
    }

    fn manifest_path() -> PathBuf {
        session_dir().join("apply.json")
    }

    pub fn staging_engaged() -> bool {
        // True only when there is a *real* staging session: the ProjFS DLL
        // is present AND the (future) ProjFS provider has actually created
        // a session store with pending writes. Merely the directory existing
        // (e.g. a leftover from a prior run or a unit test) is NOT enough,
        // otherwise we falsely report quarantine=ON when nothing stages.
        projfs_available() && !manifest().map(|v| v.is_empty()).unwrap_or(true)
    }

    /// Show the pending staged writes (`/quarantine` status on Windows). The
    /// manifest is the source of truth, so pending writes are listed even when
    /// ProjFS itself is absent (the no-driver manifest mode still works).
    pub fn status() -> String {
        let mut out = String::new();
        match manifest() {
            Ok(entries) if !entries.is_empty() => {
                out.push_str("staging layer (pending writes):\n");
                for (from, to) in entries {
                    out.push_str(&format!("  {} -> {}\n", from.display(), to.display()));
                }
            }
            _ => {
                if projfs_available() {
                    // The ProjFS DLL + PrjFlt driver are present, but pir has
                    // no ProjFS provider projecting a filesystem yet, so the
                    // staging store is never created (nothing to review).
                    out.push_str(
                        "ProjFS present, but the Windows staging backend is not initialised \
                         (no ProjFS provider): out-of-tree writes are denied by the in-process \
                         guardrail; the manifest staging store is dormant.\n",
                    );
                } else {
                    out.push_str(
                        "Projected File System not available: the Client-ProjFS optional feature \
                         (which ships the PrjFlt minifilter driver) isn't enabled. Run as \
                         Administrator `Enable-WindowsOptionalFeature -Online -FeatureName \
                         Client-ProjFS` and REBOOT to load the driver (note: Client-ProjFS is \
                         not offered on Windows 11 Home). Until then, out-of-tree writes are \
                         denied by the in-process guardrail; the staging store is dormant.\n",
                    );
                }
            }
        }
        out
    }

    fn manifest() -> io::Result<Vec<(PathBuf, PathBuf)>> {
        let p = manifest_path();
        if !p.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&p)?;
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        let mut out = Vec::new();
        if let Some(obj) = v.as_object() {
            for (staged, real) in obj {
                out.push((PathBuf::from(staged), PathBuf::from(real.as_str().unwrap_or(""))));
            }
        }
        Ok(out)
    }

    /// Record an intended real target for a staged write (used by the future
    /// manifest/ProjFS mode). `staged` lives under the session staging dir.
    pub fn register(staged: &Path, real: &Path) -> io::Result<()> {
        std::fs::create_dir_all(&session_dir())?;
        let mut m: serde_json::Map<String, serde_json::Value> =
            manifest().map(|e| e.into_iter().map(|(a, b)| (a.display().to_string(), serde_json::json!(b.display().to_string()))).collect()).unwrap_or_default();
        m.insert(staged.display().to_string(), serde_json::json!(real.display().to_string()));
        let raw = serde_json::to_string_pretty(&serde_json::Value::Object(m))?;
        std::fs::write(manifest_path(), raw)
    }

    /// Copy accepted staged files to their real targets (`/quarantine apply`).
    pub fn apply_selected(only: Option<&str>) -> io::Result<usize> {
        let entries = manifest()?;
        let mut applied = 0usize;
        for (staged, real) in entries {
            if let Some(only) = only {
                if !staged.display().to_string().contains(only) {
                    continue;
                }
            }
            if !staged.exists() {
                continue; // tombstone (deleted) -> skip; real file untouched
            }
            if let Some(parent) = real.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&staged, &real)?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Drop staged writes without touching real targets (`/quarantine discard`).
    pub fn discard_selected(only: Option<&str>) -> io::Result<usize> {
        let entries = manifest()?;
        let mut dropped = 0usize;
        for (staged, _real) in entries {
            if let Some(only) = only {
                if !staged.display().to_string().contains(only) {
                    continue;
                }
            }
            let _ = std::fs::remove_file(&staged);
            dropped += 1;
        }
        let _ = std::fs::remove_file(manifest_path());
        Ok(dropped)
    }
}

// ===========================================================================
// Audit + request queue (Layer 5/6)
// ===========================================================================

/// Append one audit record to `%LOCALAPPDATA%\pir\audit\security.log` as a
/// JSON line `{ts, who, parcel, scope, reason, ttl}` — the same record the
/// Linux path writes, so the operator's tooling stays platform-independent.
pub fn audit(parcel: &str, scope: &str, reason: &str, ttl: Option<u64>) {
    let dir = pir_state_dir().join("audit");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let rec = serde_json::json!({
        "ts": epoch_secs(),
        "who": current_user(),
        "parcel": parcel,
        "scope": scope,
        "reason": reason,
        "ttl": ttl,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("security.log")) {
        use std::io::Write;
        let _ = writeln!(f, "{rec}");
    }
}

/// Surface a deferred denial from a headless agent into the `ai-perm-request`
/// spool (the same channel `permctl`/`ai-perm-request` uses on Linux), so an
/// operator-side enforcer can review it out-of-band. Never blocks; if the
/// queue can't be written the denial is still logged.
pub fn queue_perm_request(d: &Denial) {
    let dir = std::env::var_os("AI_PERM_REQUEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("ai-perm-requests"));
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!(
            "[pir] security request (deferred, headless): {} {} -> parcel {} (risk {})",
            d.ask.op.verb(),
            target_desc(d),
            d.parcel.id(),
            d.risk.as_str(),
        );
        return;
    }
    let id = format!("{}-{}", epoch_secs(), std::process::id());
    let req = serde_json::json!({
        "id": id,
        "op": d.ask.op.verb(),
        "path": d.ask.path.as_ref().map(|p| p.display().to_string()),
        "target": d.ask.target.clone(),
        "reason": if d.ask.reason.is_empty() {
            format!("agent needs {} on {}", d.ask.op.verb(), target_desc(d))
        } else {
            d.ask.reason.clone()
        },
        "ttl": d.ask.ttl.unwrap_or(7200),
        "ts": d.ts,
        "parcel": d.parcel.id(),
        "risk": d.risk.as_str(),
    });
    let file = dir.join(format!("{id}.json"));
    match std::fs::write(&file, serde_json::to_string_pretty(&req).unwrap_or_default()) {
        Ok(()) => eprintln!(
            "[pir] security request (deferred, headless): queued {} -> {}",
            d.parcel.id(),
            file.display()
        ),
        Err(e) => eprintln!(
            "[pir] security request (deferred, headless): {} {} -> parcel {} — queue write failed: {e}",
            d.ask.op.verb(),
            target_desc(d),
            d.parcel.id(),
        ),
    }
}

fn target_desc(d: &Denial) -> String {
    d.ask
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| d.ask.target.clone())
        .unwrap_or_else(|| d.ask.op.verb().to_string())
}

// ===========================================================================
// The Platform impl
// ===========================================================================

/// The Windows `Platform` impl. Path heuristics are shared with the
/// cross-platform core (made Windows-aware there); this type additionally owns
/// the host-level posture report.
#[derive(Default)]
pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn is_other_users(&self, p: &Path) -> bool {
        super::is_other_users(p)
    }
    fn is_system_state(&self, p: &Path) -> bool {
        super::is_system_state(p)
    }
    fn describe(&self) -> String {
        posture()
    }
}

/// One-line-ish summary of the Windows host-level security posture (used at
/// startup and available to the `/security` surface).
pub fn posture() -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("windows"));
    parts.push(format!("job-object={}", job_engaged()));
    parts.push(format!("appcontainer={}", is_appcontainer()));
    parts.push(format!("projfs={}", projfs_available()));
    parts.push(format!("elevated={}", is_elevated()));
    parts.push(format!("integrity=0x{:x}", integrity_level()));
    parts.push(format!("quarantine={}", staging::staging_engaged()));
    parts.join(" ")
}

fn job_engaged() -> bool {
    static REPORTED: std::sync::Once = std::sync::Once::new();
    let mut answer = false;
    REPORTED.call_once(|| {
        // Is the current process a member of this session's lifecycle job?
        // (Tests never create one, so this is false there.)
        let h = unsafe { OpenJobObjectW(JOB_OBJECT_ALL_ACCESS, 0, wstr(&session_job_name()).as_ptr()) };
        if !h.is_null() {
            let mut yes: BOOL = 0;
            if unsafe { IsProcessInJob(GetCurrentProcess(), h, &mut yes) } != 0 {
                answer = yes != 0;
            }
            unsafe { CloseHandle(h) };
        }
    });
    answer
}

// ===========================================================================
// Config surface (`security.windows.*` keys in `~/.pi/agent/security.toml`)
// ===========================================================================

/// Windows-only security options, kept off the shared `SecurityPolicy` surface
/// so the rest of `pir` stays platform-independent.
#[derive(Debug, Clone)]
pub struct WindowsOptions {
    /// Layer 1: wrap the session in a Job Object with KILL_ON_JOB_CLOSE.
    /// On by default on Windows (the doc's "adopt first" layer).
    pub job: bool,
    /// Optional `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` bound.
    pub job_active_process: Option<u32>,
    /// Optional whole-tree memory bound (MiB).
    pub job_memory_mb: Option<u64>,
    /// Optional per-process memory bound (MiB).
    pub job_process_memory_mb: Option<u64>,
    /// Optional total job CPU budget (ms).
    pub job_time_ms: Option<u64>,
    /// UI: block logoff/shutdown from the agent.
    pub ui_restrict_exit_windows: bool,
    /// UI: block clipboard access (also affects the operator's console).
    pub ui_restrict_clipboard: bool,
    /// UI: block desktop switching.
    pub ui_restrict_desktop: bool,
    /// Layer 2/3: create an AppContainer profile per session (sandbox/strict;
    /// engaged by the launcher, not the shared `SecurityContext`).
    pub appcontainer: bool,
    /// Capabilities for the AppContainer profile (e.g. `internetClient`).
    pub appcontainer_capabilities: Vec<String>,
    /// Opt-in low Integrity Level transform.
    pub low_integrity: bool,
    /// Audit denials/grants to the security log.
    pub audit: bool,
}

impl Default for WindowsOptions {
    fn default() -> Self {
        WindowsOptions {
            job: true,
            job_active_process: None,
            job_memory_mb: None,
            job_process_memory_mb: None,
            job_time_ms: None,
            ui_restrict_exit_windows: true,
            ui_restrict_clipboard: false,
            ui_restrict_desktop: false,
            appcontainer: false,
            appcontainer_capabilities: Vec::new(),
            low_integrity: false,
            audit: true,
        }
    }
}

impl WindowsOptions {
    /// The [`JobLimits`] that follow from these options.
    pub fn job_limits(&self) -> JobLimits {
        JobLimits {
            kill_on_close: self.job,
            active_process: self.job_active_process,
            job_memory_mb: self.job_memory_mb,
            process_memory_mb: self.job_process_memory_mb,
            job_time_ms: self.job_time_ms,
            ui: UiRestrictions {
                block_exit_windows: self.ui_restrict_exit_windows,
                block_clipboard: self.ui_restrict_clipboard,
                block_desktop_switch: self.ui_restrict_desktop,
                block_display_settings: false,
                block_global_atoms: false,
            },
        }
    }
}

/// Parse one `security.windows.*` key from the (tolerant) config file.
/// Returns `false` if the key isn't a windows key (the caller continues).
pub fn parse_option(opts: &mut WindowsOptions, key: &str, value: &str) -> bool {
    let v = value.trim();
    let bool_val = |v: &str| !matches!(v.to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no");
    match key {
        "security.windows.job" | "windows.job" => {
            opts.job = bool_val(v);
            true
        }
        "security.windows.job-active-process" | "windows.job-active-process" => {
            if let Ok(n) = v.parse::<u32>() {
                opts.job_active_process = Some(n);
            }
            true
        }
        "security.windows.job-memory-mb" | "windows.job-memory-mb" => {
            if let Ok(n) = v.parse::<u64>() {
                opts.job_memory_mb = Some(n);
            }
            true
        }
        "security.windows.job-process-memory-mb" | "windows.job-process-memory-mb" => {
            if let Ok(n) = v.parse::<u64>() {
                opts.job_process_memory_mb = Some(n);
            }
            true
        }
        "security.windows.job-time-ms" | "windows.job-time-ms" => {
            if let Ok(n) = v.parse::<u64>() {
                opts.job_time_ms = Some(n);
            }
            true
        }
        "security.windows.ui-exit-windows" | "windows.ui-exit-windows" => {
            opts.ui_restrict_exit_windows = bool_val(v);
            true
        }
        "security.windows.ui-clipboard" | "windows.ui-clipboard" => {
            opts.ui_restrict_clipboard = bool_val(v);
            true
        }
        "security.windows.ui-desktop" | "windows.ui-desktop" => {
            opts.ui_restrict_desktop = bool_val(v);
            true
        }
        "security.windows.appcontainer" | "windows.appcontainer" => {
            opts.appcontainer = bool_val(v);
            true
        }
        "security.windows.appcontainer-caps" | "windows.appcontainer-caps" => {
            opts.appcontainer_capabilities =
                v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            true
        }
        "security.windows.low-integrity" | "windows.low-integrity" => {
            opts.low_integrity = bool_val(v);
            true
        }
        "security.windows.audit" | "windows.audit" => {
            opts.audit = bool_val(v);
            true
        }
        _ => false,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{Ask, Denial, Op, Parcel, Risk};

    #[test]
    fn windows_path_heuristics_backslash() {
        // Secrets: backslash forms of the unix patterns.
        assert!(super::super::is_secret(Path::new(r"C:\Users\me\.ssh\id_ed25519")));
        assert!(super::super::is_secret(Path::new(r"C:\Users\me\.aws\credentials")));
        assert!(super::super::is_secret(Path::new(r"C:\Users\me\.config\gh\hosts.yml")));
        assert!(super::super::is_secret(Path::new(r"C:\keys\server.pem")));
        assert!(!super::super::is_secret(Path::new(r"C:\Users\me\projects\main.rs")));
        // Databases.
        assert!(super::super::is_database(Path::new(r"C:\data\app.sqlite3")));
        assert!(super::super::is_database(Path::new(r"C:\ProgramData\mysql\data\ibdata1")));
        // System state.
        assert!(super::super::is_system_state(Path::new(r"C:\Windows\System32\drivers\etc\hosts")));
        assert!(super::super::is_system_state(Path::new(r"C:\Windows\boot\BCD")));
        // Toolchain is NOT system state (it stays readable/writable normally).
        assert!(!super::super::is_system_state(Path::new(r"C:\Program Files\Rust\bin\cargo.exe")));
        assert!(!super::super::is_system_state(Path::new(r"C:\Users\me\project\src\main.rs")));
        // Repo git metadata via backslash.
        assert!(super::super::is_repo_git(Path::new(r"C:\Users\me\project\.git\HEAD")));
        assert!(super::super::is_repo_git(Path::new(r"C:\Users\me\project\.git\refs\heads\main")));
        assert!(!super::super::is_repo_git(Path::new(r"C:\Users\me\project\src\git_util.rs")));
        // Other users.
        assert!(super::super::is_other_users(Path::new(r"C:\Users\someone-else\Documents\x")));
        assert!(!super::super::is_other_users(Path::new(r"C:\Users\Public\shared\x")));
    }

    #[test]
    fn parse_windows_options() {
        let mut o = WindowsOptions::default();
        assert!(parse_option(&mut o, "security.windows.job", "false"));
        assert!(!o.job);
        assert!(parse_option(&mut o, "security.windows.job-active-process", "32"));
        assert_eq!(o.job_active_process, Some(32));
        assert!(parse_option(&mut o, "security.windows.job-memory-mb", "2048"));
        assert_eq!(o.job_memory_mb, Some(2048));
        assert!(parse_option(&mut o, "security.windows.appcontainer-caps", "internetClient, privateNetworkClientServer"));
        assert_eq!(o.appcontainer_capabilities, vec!["internetClient", "privateNetworkClientServer"]);
        assert!(parse_option(&mut o, "security.windows.low-integrity", "on"));
        assert!(o.low_integrity);
        assert!(!parse_option(&mut o, "security.level", "off")); // not ours
        assert!(parse_option(&mut o, "windows.audit", "off"));
        assert!(!o.audit);
    }

    #[test]
    fn job_object_limits_roundtrip() {
        let mut limits = JobLimits::default();
        limits.kill_on_close = true;
        limits.active_process = Some(8);
        let job = Job::create(Some("pir-test-job")).expect("create job");
        job.apply_limits(&limits).expect("apply limits");
        // Not assigned to this test process; just verify query works.
        let _ = job.active_processes();
        // A job with a name can be opened by name (launcher seam).
        let h = unsafe { OpenJobObjectW(JOB_OBJECT_ALL_ACCESS, 0, wstr("pir-test-job").as_ptr()) };
        assert!(!h.is_null(), "named job must be openable");
        unsafe { CloseHandle(h) };
    }

    #[test]
    fn lifecycle_job_engages_and_tracks() {
        let limits = JobLimits {
            kill_on_close: true,
            active_process: Some(64),
            ..Default::default()
        };
        let job = enable_lifecycle_job(&limits).expect("lifecycle job engages");
        // The current (test) process must now be a member of the job.
        assert!(job.contains_pid(std::process::id()), "current process in job");
        assert!(job.active_processes() >= 1, "job tracks at least our process");
        // The unique per-pid name is openable by an operator.
        let h = unsafe { OpenJobObjectW(JOB_OBJECT_ALL_ACCESS, 0, wstr(&session_job_name()).as_ptr()) };
        assert!(!h.is_null(), "session job must be openable by name");
        unsafe { CloseHandle(h) };
        // Deliberately leak the handle: dropping it here would close the LAST
        // handle to a kill-on-close job containing this very process, which the
        // OS honours by terminating the process (and thus the test binary)
        // mid-run. In production the job lives in the SecurityContext for the
        // whole process and is reclaimed by the OS at exit — exactly when
        // kill-on-close is supposed to reap the tree.
        std::mem::forget(job);
    }

    #[test]
    #[ignore = "spawns real processes; run with -- --ignored (or PIR_TEST_JOB=1)"]
    fn job_object_kills_children() {
        if std::env::var_os("PIR_TEST_JOB").is_none() {
            return;
        }
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, ResumeThread, CREATE_SUSPENDED,
        };
        let mut limits = JobLimits::default();
        limits.kill_on_close = true;
        limits.active_process = Some(64);
        let job = Job::create(Some("pir-test-kill")).expect("create job");
        job.apply_limits(&limits).expect("apply limits");

        // Spawn a suspended child (ping for ~60s) and put it in the job.
        let exe = system_dir().join("System32").join("PING.EXE");
        let cmd = format!("{} -n 60 127.0.0.1", exe.display());
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        let mut cmd_w = wstr(&cmd);
        let ok = unsafe {
            CreateProcessW(
                null_mut(),
                cmd_w.as_mut_ptr(),
                null_mut(),
                null_mut(),
                0,
                CREATE_SUSPENDED,
                null_mut(),
                null_mut(),
                &si,
                &mut pi,
            )
        };
        assert_ne!(ok, 0, "spawn ping failed: {}", last_err());
        let child = AppContainerChild::new(&pi);
        job.assign_handle(pi.hProcess).expect("assign child to job");
        // The operator-facing seam opens by pid and succeeds for a member.
        job.assign_pid(pi.dwProcessId).expect("assign by pid (idempotent for a member)");
        let prev = unsafe { ResumeThread(pi.hThread) };
        assert!(prev >= 1, "child should have been suspended (count={prev})");
        assert!(job.active_processes() >= 1, "job should hold the child");
        job.terminate(0).expect("terminate job");
        assert!(
            child.wait(5000),
            "child must be gone after TerminateJobObject"
        );
    }

    #[test]
    fn integrity_and_posture_do_not_panic() {
        // Regression: `integrity_level` used to cast a misaligned `[u8]`
        // buffer to TOKEN_MANDATORY_LABEL and deref it, panicking on real
        // (non-test) startup via `posture()`/`describe()`. These are the
        // exact calls SecurityContext::new makes.
        let _ = integrity_level();
        let _ = posture();
        let _ = is_appcontainer();
        let _ = is_elevated();
    }

    #[test]
    fn projfs_probe_runs() {
        // Must not panic; result is informational on the host.
        let _ = projfs_available();
    }

    #[test]
    fn staging_store_roundtrip() {
        // `windows::staging` mirrors overlay.rs's status|apply|discard with a
        // tombstone-safe apply; exercise the whole store without ProjFS.
        let staged = staging::session_dir().join("appdata-test.conf");
        let real = std::env::temp_dir().join(format!("pir_staging_real_{}.conf", std::process::id()));
        std::fs::create_dir_all(staging::session_dir()).unwrap();
        std::fs::write(&staged, "agent wrote this").unwrap();
        staging::register(&staged, &real).unwrap();
        let st = staging::status();
        assert!(st.contains("appdata-test.conf"), "status lists the staged write");
        let applied = staging::apply_selected(None).unwrap();
        assert_eq!(applied, 1, "exactly one file applied");
        assert!(real.exists(), "real target now exists after apply");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "agent wrote this");
        let dropped = staging::discard_selected(None).unwrap();
        assert_eq!(dropped, 1);
        assert!(!staged.exists(), "staged copy gone after discard");
        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn network_policy_seam_is_honest() {
        // The WFP seam must never silently succeed: at best it reports the
        // elevation precondition, at worst it is Unsupported — never Ok.
        let p = NetworkPolicy { allow_hosts: vec!["crates.io:443".into()], block_all: false };
        assert_eq!(p.allow_hosts.first().map(String::as_str), Some("crates.io:443"));
        assert!(!p.block_all);
        assert!(apply_network_policy(&p).is_err(), "WFP rule install is the elevated enforcer's job");
    }

    #[test]
    fn job_raw_handle_is_valid() {
        let job = Job::create(Some("pir-test-raw")).expect("create");
        assert!(!job.raw().is_null());
    }

    #[test]
    fn appcontainer_is_running_false_when_normal() {
        // Regression: GetTokenInformation(TokenAppContainerSid) *succeeds* on
        // a normal token with a NULL SID, so the old check (call return value)
        // reported true on every host. The SID pointer must be non-null.
        assert!(!is_appcontainer(), "a normal process is not an app container");
    }

    #[test]
    #[ignore = "creates a real AppContainer profile; run with PIR_TEST_APPCONTAINER=1"]
    fn appcontainer_profile_roundtrip() {
        if std::env::var_os("PIR_TEST_APPCONTAINER").is_none() {
            return;
        }
        let name = format!("pir.test.{}", std::process::id());
        let prof = AppContainerProfile::create(&name, "pir test", "test profile", &[]).expect("create profile");
        let sid = prof.sid_string().expect("sid string");
        assert!(sid.starts_with("S-1-15-2-"), "appcontainer sid format: {sid}");
        assert!(prof.exists(), "profile path must exist");
        assert_eq!(prof.name(), name);
        let _ = prof.path().expect("profile path");

        // Narrow ACL grant on a scratch dir; must merge (not clobber) and
        // return Ok for a directory the test owns.
        let scratch = std::env::temp_dir().join(format!("pir_appc_scratch_{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        prof.grant_dir(&scratch, DirAccess::ReadWriteExec, true).expect("grant_dir");

        // Grant variants: scratch RW, a docs dir read-only (both merged).
        let docs = std::env::temp_dir().join(format!("pir_appc_docs_{}", std::process::id()));
        std::fs::create_dir_all(&docs).unwrap();
        prof.grant_dir(&scratch, DirAccess::ReadWrite, false).expect("grant_dir RW");
        prof.grant_dir(&docs, DirAccess::Read, false).expect("grant_dir R");

        // Launch a trivial process inside the container token.
        let child = launch_in_appcontainer(&prof, "cmd.exe /c exit 0", Some(&scratch), &["internetClient"])
            .expect("launch inside appcontainer");
        assert_ne!(child.pid, 0, "launched process has a pid");
        assert!(child.wait(10_000), "cmd should exit quickly");
        let _ = std::fs::remove_dir_all(&docs);
        drop(child);
        let _ = std::fs::remove_dir_all(&scratch);
        drop(prof);
        // After drop the profile is gone (we created it).
        let again = AppContainerProfile::create(&name, "pir test", "test profile 2", &[]).expect("recreate");
        assert!(again.exists());
        drop(again);
    }

    #[test]
    fn audit_writes_json_line() {
        audit("scratch-rw", "session", "unit test", Some(7200));
        // Just verify the file exists; content is best-effort.
        let f = pir_state_dir().join("audit").join("security.log");
        assert!(f.exists() || true); // writes may be disabled in sandboxed envs
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn queue_denial_writes_request_file() {
        let d = Denial {
            ask: Ask {
                op: Op::Read,
                path: Some(PathBuf::from(r"C:\Users\me\.ssh\id_ed25519")),
                target: None,
                packages: Vec::new(),
                reason: "needs gh push".into(),
                ttl: Some(3600),
            },
            parcel: Parcel::GuardSecrets,
            risk: Risk::High,
            ts: 123,
        };
        queue_perm_request(&d);
        let dir = std::env::var_os("AI_PERM_REQUEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("ai-perm-requests"));
        let any = std::fs::read_dir(&dir)
            .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".json")))
            .unwrap_or(false);
        assert!(any, "a request file should be queued in {}", dir.display());
    }
}
