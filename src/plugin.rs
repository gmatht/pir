//! Core plugin ABI (extension type "a": statically compiled Rust extensions).
//!
//! Every tool — built-in or dropped-in — is exposed to the model through one
//! uniform contract: a [`ToolBackend`] that lists [`ToolSpec`]s and runs them,
//! returning an [`Outcome`]. Extensions are linked at compile time by
//! `build.rs`, which scans `extensions/*/src/lib.rs` and emits a
//! `register_all` that pushes each backend into a [`Registry`].
//!
//! # Writing an extension
//!
//! Drop a folder into `extensions/<name>/` containing `src/lib.rs`:
//!
//! ```rust,ignore
//! use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
//! use serde_json::json;
//!
//! pub fn register(reg: &mut Registry) {
//!     reg.add(Box::new(MyExt));
//! }
//!
//! struct MyExt;
//! impl ToolBackend for MyExt {
//!     fn name(&self) -> &'static str { "my-ext" }
//!     fn specs(&self) -> Vec<ToolSpec> {
//!         vec![ToolSpec {
//!             name: "hello",
//!             description: "say hello",
//!             schema: json!({ "type": "object", "properties": {}, "required": [] }),
//!         }]
//!     }
//!     fn run(&mut self, name: &str, _input: &serde_json::Value) -> Outcome {
//!         match name {
//!             "hello" => Outcome::ok("hello".into()),
//!             other => Outcome::err(format!("unknown tool '{other}'")),
//!         }
//!     }
//! }
//! ```
//!
//! That's it — rebuild and the tool appears in the model's tool list. Because
//! the extension is compiled into `pir`, it can call any `crate::*` module
//! (e.g. `crate::term`, `crate::config`). External/binary/lua/http
//! extensions are themselves just more Rust extensions that spawn processes or
//! embed an interpreter — the core never special-cases them.

use serde_json::Value;
use std::path::PathBuf;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
}

impl Outcome {
    pub fn ok(content: String) -> Self {
        Outcome { content, is_error: false }
    }
    pub fn err(content: String) -> Self {
        Outcome { content, is_error: true }
    }
}

/// A source of one or more tools. Implement this in an extension and register
/// it via [`Registry::add`] from your `register` entry point.
pub trait ToolBackend: Send {
    /// Human-readable extension name (for logging).
    fn name(&self) -> &'static str;
    /// Tools this backend provides.
    fn specs(&self) -> Vec<ToolSpec>;
    /// Run a tool by name. Return [`Outcome::err`] for unknown names.
    fn run(&mut self, name: &str, input: &Value) -> Outcome;
}

/// Holds every linked backend. The model only ever sees `specs()`; it never
/// knows (or cares) which extension a tool came from.
pub struct Registry {
    cwd: PathBuf,
    full_auto: bool,
    backends: Vec<Box<dyn ToolBackend>>,
}

impl Registry {
    pub fn new(cwd: PathBuf, full_auto: bool) -> Self {
        Registry { cwd, full_auto, backends: Vec::new() }
    }

    /// Link an extension's backend.
    pub fn add(&mut self, backend: Box<dyn ToolBackend>) {
        self.backends.push(backend);
    }

    /// Flat list of every tool across all backends — sent to the model.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.backends.iter().flat_map(|b| b.specs()).collect()
    }

    pub fn execute(&mut self, name: &str, input: &Value) -> Outcome {
        for b in &mut self.backends {
            if b.specs().iter().any(|s| s.name == name) {
                return b.run(name, input);
            }
        }
        Outcome::err(format!("unknown tool '{name}'"))
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }
    pub fn full_auto(&self) -> bool {
        self.full_auto
    }
    pub fn len(&self) -> usize {
        self.backends.len()
    }
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
}

/// Shared helper: truncate a string in place to `max_chars`, appending a
/// marker. Used by backends that may return large text.
pub fn truncate(s: &mut String, max_chars: usize) {
    if s.chars().count() > max_chars {
        let cut = s.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push_str("\n… [pir] output truncated");
    }
}
