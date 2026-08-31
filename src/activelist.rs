//! Named active-session lists (save/restore).
//!
//! Backgrounded sessions are in-memory (`BackgroundJobs` in `main.rs`) and lost
//! on exit. This module persists the set of sessions the user is actively
//! tracking to `~/.pi/active-sessions/<name>.json` (a dedicated directory,
//! sibling of `~/.pi/agent/`), so the "drive the queue" flow survives a restart.
//!
//! A list has a **name** (e.g. `default`, `work`, `side-project`). The dialog
//! exposes Save (write back to the loaded list), Save As (pick a new name), and
//! Load (pick a list to restore).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One entry in an active-session list: the session log path + a short label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSession {
    pub log: PathBuf,
    pub label: String,
}

/// A named active-session list.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActiveList {
    pub name: String,
    pub sessions: Vec<ActiveSession>,
}

/// The directory holding named lists: `~/.pi/active-sessions/`.
pub fn lists_dir() -> PathBuf {
    crate::config::pi_dir().join("active-sessions")
}

/// Path to a named list file.
pub fn list_path(name: &str) -> PathBuf {
    lists_dir().join(format!("{name}.json"))
}

/// List the names of all saved active-session lists (sorted).
pub fn list_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(read) = std::fs::read_dir(lists_dir()) {
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".json") {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// Load a named list. Returns `None` if it doesn't exist or can't be read.
pub fn load(name: &str) -> Option<ActiveList> {
    let raw = std::fs::read_to_string(list_path(name)).ok()?;
    let mut list: ActiveList = serde_json::from_str(&raw).ok()?;
    list.name = name.to_string();
    Some(list)
}

/// Save a list to its file (creating the directory if needed).
pub fn save(list: &ActiveList) -> std::io::Result<()> {
    std::fs::create_dir_all(lists_dir())?;
    let raw = serde_json::to_string_pretty(list).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(list_path(&list.name), raw)
}

/// Save the current in-memory list back to the loaded list's file. Returns the
/// name saved to.
pub fn save_back(list: &ActiveList) -> std::io::Result<String> {
    save(list)?;
    Ok(list.name.clone())
}

/// Save the current sessions under a new name (Save As). After this, the new
/// name becomes the loaded list.
pub fn save_as(list: &ActiveList, new_name: &str) -> std::io::Result<String> {
    let mut l = list.clone();
    l.name = new_name.to_string();
    save(&l)?;
    Ok(new_name.to_string())
}
