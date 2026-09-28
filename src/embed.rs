//! Embedded credentials + model store ("self-contained pir").
//!
//! `pir --embed <dir>` writes a *copy* of the running pir executable to
//! `<dir>/pire` with the operator's `auth.json` and `models-store.json`
//! appended after the ELF image, inside a self-describing trailer. Running
//! `<dir>/pire` (or any pir that has such a payload appended to itself)
//! transparently reads the trailer and merges the embedded providers/keys
//! *under* the local config, so a shared/copied binary still works while a
//! machine-local store or key keeps winning on conflict.
//!
//! Framing (little-endian, appended at the very end of the file):
//!
//! ```text
//!   ...executable bytes...
//!   <entry table>     count (u64), then per entry:
//!                       name_len (u32), data_len (u64), name, data
//!   MAGIC             b"PIREMBD\x01"   (8 bytes)
//!   table_offset      u64, byte offset of <entry table> from file start
//! ```
//!
//! The magic and the table offset are the *last* 16 bytes of the file, so a
//! reader locates the payload with a single seek to `filesize - 16` and never
//! needs to know the executable's real length. The offset is self-describing
//! rather than derived from the on-disk size, so alignment padding or an
//! appended second payload cannot confuse the reader: the newest trailer always
//! wins because it owns the file tail.

use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 8-byte trailer magic, last byte is a framing version.
const MAGIC: &[u8; 8] = b"PIREMBD\x01";
/// Fixed tail size: MAGIC (8) + table_offset (u64, 8).
const TAIL_LEN: u64 = 16;

/// The embedded document names. These mirror the two files under
/// `~/.pi/agent/` that pir reads for credentials and the model catalog.
pub const ENTRY_AUTH: &str = "auth.json";
pub const ENTRY_STORE: &str = "models-store.json";

/// One appended document (name plus its verbatim bytes).
pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
}

/// Build the framed trailer bytes for `entries` (excluding the magic tail,
/// which [`write_payload`] appends). Exposed for unit tests.
fn build_table(entries: &[Entry]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for e in entries {
        buf.extend_from_slice(&(e.name.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(e.data.len() as u64).to_le_bytes());
        buf.extend_from_slice(e.name.as_bytes());
        buf.extend_from_slice(&e.data);
    }
    buf
}

/// Append `entries` as a payload trailer to a copy of the executable at
/// `src_exe`, writing the result to `dest` (e.g. `<dir>/pire`).
///
/// The executable bytes are copied verbatim first, then the trailer is
/// appended. Any pre-existing trailer in `src_exe` is irrelevant: the copy is
/// of the *pristine* running binary (which is not itself embedded unless the
/// operator embedded twice; even then the newest trailer wins on read because
/// we anchor at the file tail).
pub fn write_embedded(dest: &Path, entries: Vec<Entry>) -> Result<(), String> {
    let src_exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate the running pir executable: {e}"))?;
    let mut out = fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    // Preserve the source's executable bit.
    #[cfg(unix)]
    if let Ok(meta) = fs::metadata(&src_exe) {
        let _ = fs::set_permissions(dest, meta.permissions());
    }
    let mut src =
        fs::File::open(&src_exe).map_err(|e| format!("cannot read {}: {e}", src_exe.display()))?;
    let copied = std::io::copy(&mut src, &mut out)
        .map_err(|e| format!("cannot copy {}: {e}", src_exe.display()))?;

    // The table sits immediately after the executable bytes, so its offset is
    // exactly what we copied — measured, not guessed from the source's stat
    // size, which would be wrong if `src_exe` were itself an embedded copy.
    let table = build_table(&entries);
    let mut tail = Vec::with_capacity(table.len() + TAIL_LEN as usize);
    tail.extend_from_slice(&table);
    tail.extend_from_slice(MAGIC);
    tail.extend_from_slice(&copied.to_le_bytes());
    out.write_all(&tail)
        .map_err(|e| format!("cannot write trailer to {}: {e}", dest.display()))?;
    out.flush()
        .map_err(|e| format!("cannot flush {}: {e}", dest.display()))?;
    Ok(())
}

/// Read the embedded payload trailer from `exe` (typically the running
/// executable). Returns the raw bytes of each named entry, or `None` when
/// there is no valid trailer (i.e. a normal, non-embedded pir binary).
pub fn read_payload(exe: &Path) -> Option<Vec<(String, Vec<u8>)>> {
    let mut f = fs::File::open(exe).ok()?;
    let size = f.metadata().ok()?.len();
    if size < TAIL_LEN {
        return None;
    }
    // Read the fixed 16-byte tail: MAGIC + table_offset.
    f.seek(SeekFrom::End(-(TAIL_LEN as i64))).ok()?;
    let mut tail = [0u8; TAIL_LEN as usize];
    f.read_exact(&mut tail).ok()?;
    if &tail[0..8] != MAGIC {
        return None;
    }
    let table_offset = u64::from_le_bytes(tail[8..16].try_into().ok()?);
    if table_offset == 0 || table_offset >= size {
        return None;
    }
    f.seek(SeekFrom::Start(table_offset)).ok()?;

    let mut count_buf = [0u8; 8];
    f.read_exact(&mut count_buf).ok()?;
    let count = u64::from_le_bytes(count_buf);
    // A sanity bound so a corrupt length cannot make us allocate wildly.
    if count == 0 || count > 64 {
        return None;
    }
    let mut out = Vec::new();
    for _ in 0..count {
        let mut len_buf = [0u8; 12];
        f.read_exact(&mut len_buf).ok()?;
        let name_len = u32::from_le_bytes(len_buf[0..4].try_into().ok()?) as usize;
        let data_len = u64::from_le_bytes(len_buf[4..12].try_into().ok()?);
        if name_len == 0 || name_len > 256 || data_len > 32 * 1024 * 1024 {
            return None;
        }
        let mut name = vec![0u8; name_len];
        f.read_exact(&mut name).ok()?;
        let name = String::from_utf8(name).ok()?;
        let mut data = vec![0u8; data_len as usize];
        f.read_exact(&mut data).ok()?;
        out.push((name, data));
    }
    Some(out)
}

/// Read this process's own executable and return the embedded documents
/// (name → parsed JSON). Empty when the running binary has no trailer.
fn self_payload() -> Vec<(String, Value)> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let Some(entries) = read_payload(&exe) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|(name, data)| {
            let v: Value = serde_json::from_slice(&data).ok()?;
            Some((name, v))
        })
        .collect()
}

use std::sync::OnceLock;
static SELF_PAYLOAD: OnceLock<Vec<(String, Value)>> = OnceLock::new();

/// The embedded documents of the *running* pir binary, parsed once. Each item
/// is `(document-name, parsed-json)`. Empty for a normal binary.
pub fn embedded_documents() -> &'static [(String, Value)] {
    SELF_PAYLOAD.get_or_init(self_payload)
}

/// The embedded `models-store.json` (if the running binary has one), as a
/// parsed [`Value`]. `None` when absent or unparseable.
pub fn embedded_store() -> Option<&'static Value> {
    embedded_documents()
        .iter()
        .find(|(n, _)| n == ENTRY_STORE)
        .map(|(_, v)| v)
}

/// The embedded `auth.json` (if the running binary has one), as a parsed
/// [`Value`]. `None` when absent or unparseable.
pub fn embedded_auth() -> Option<&'static Value> {
    embedded_documents()
        .iter()
        .find(|(n, _)| n == ENTRY_AUTH)
        .map(|(_, v)| v)
}

// ---------------------------------------------------------------------------
// Payload construction from the operator's live config
// ---------------------------------------------------------------------------

/// Collect the documents to embed from the *current* config dir: the
/// `auth.json` and `models-store.json` under `agent_dir`. Missing files are
/// skipped (nothing to embed for them).
pub fn entries_from_config(agent_dir: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for name in [ENTRY_AUTH, ENTRY_STORE] {
        if let Ok(data) = fs::read(agent_dir.join(name)) {
            entries.push(Entry {
                name: name.to_string(),
                data,
            });
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// Merging: embedded store is layered UNDER the local one (local wins)
// ---------------------------------------------------------------------------

/// Merge the embedded providers into `local` (a parsed `models-store.json`
/// `Value`) and return the combined value. The local store's provider entries
/// win on id collision; embedded entries only contribute providers the local
/// store lacks, plus any *models* the local provider is missing.
///
/// The store's `providers` may be an array or an object keyed by id; both
/// shapes are normalised to arrays for the merge and the local shape is
/// preserved on output.
pub fn merge_store(local: &Value, embedded: &Value) -> Value {
    let (larr, earr) = (
        provider_entries(local),
        provider_entries(embedded),
    );
    if earr.is_empty() {
        return local.clone();
    }
    if larr.is_empty() {
        // No local providers at all — the embedded store is the whole store.
        return embedded.clone();
    }

    let mut merged: Vec<Value> = Vec::new();
    // Index embedded by provider id for lookup during the local pass.
    let embedded_by_id: std::collections::HashMap<String, &Value> = earr
        .iter()
        .filter_map(|p| provider_id(p).map(|id| (id.to_ascii_lowercase(), p)))
        .collect();

    let mut consumed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for lp in &larr {
        let id = provider_id(lp).unwrap_or("").to_ascii_lowercase();
        let mut local_p = lp.clone();
        if let Some(ep) = embedded_by_id.get(&id) {
            consumed.insert(id.clone());
            // Union the model lists: keep every local model, add embedded
            // models the local provider is missing. This lets an embedded
            // store contribute new models to a locally-defined provider
            // without overriding the local baseUrl/key.
            let local_models = model_ids(lp);
            let extra: Vec<Value> = model_values(ep)
                .into_iter()
                .filter(|m| {
                    model_id(m)
                        .map(|mid| !local_models.contains(&mid.to_ascii_lowercase()))
                        .unwrap_or(false)
                })
                .collect();
            if !extra.is_empty() {
                append_models(&mut local_p, extra);
            }
            // Fill in a baseUrl/apiKey the local entry omits from the embedded
            // one (a local provider with no key still gets a usable one).
            fill_if_absent(&mut local_p, ep, &["baseUrl", "base_url", "url"]);
            fill_if_absent(&mut local_p, ep, &["apiKey", "key"]);
            fill_if_absent(&mut local_p, ep, &["api"]);
        }
        merged.push(local_p);
    }
    // Any embedded provider not present locally is added wholesale.
    for ep in &earr {
        let id = provider_id(ep).unwrap_or("").to_ascii_lowercase();
        if !consumed.contains(&id) && !merged.iter().any(|p| {
            provider_id(p)
                .map(|x| x.to_ascii_lowercase() == id)
                .unwrap_or(false)
        }) {
            merged.push((*ep).clone());
        }
    }

    // Reassemble under the local store's shape.
    let mut out = local.clone();
    set_provider_entries(&mut out, merged);
    out
}

/// Merge embedded auth *keys* under the local auth map: local entries win, and
/// embedded providers the local auth lacks are added. Returns the combined
/// auth `Value` (a top-level object of provider-id → credential).
pub fn merge_auth(local: &Value, embedded: &Value) -> Value {
    let mut obj = match local.as_object() {
        Some(o) => o.clone(),
        None => return embedded.clone(),
    };
    if let Some(eobj) = embedded.as_object() {
        for (id, cred) in eobj {
            // Local wins: only insert when the local auth has no such entry
            // (or a null/empty placeholder).
            let local_missing = match obj.get(id) {
                None => true,
                Some(v) => v.is_null() || v.get("key").and_then(Value::as_str).map(|k| k.is_empty()).unwrap_or(false),
            };
            if local_missing {
                obj.insert(id.clone(), cred.clone());
            }
        }
    }
    Value::Object(obj)
}

// ----- small Value helpers -------------------------------------------------

/// The provider entries in a store `Value`, as a slice of individual provider
/// objects. Handles the `{"providers": [...]}` wrapper, a bare array, and the
/// `{"providers": {...}}` object-keyed-by-id shape.
fn provider_entries(v: &Value) -> Vec<Value> {
    let providers = v.get("providers").unwrap_or(v);
    match providers {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .iter()
            .map(|(k, pv)| {
                if pv.is_object() && pv.get("id").is_none() {
                    let mut c = pv.clone();
                    if let Some(m) = c.as_object_mut() {
                        m.insert("id".into(), Value::String(k.clone()));
                    }
                    c
                } else {
                    pv.clone()
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn provider_id(p: &Value) -> Option<&str> {
    p.get("id").or(p.get("name")).and_then(Value::as_str)
}

fn model_values(p: &Value) -> Vec<Value> {
    match p.get("models") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Object(o)) => o
            .iter()
            .map(|(k, mv)| {
                if mv.is_object() && mv.get("id").is_none() {
                    let mut c = mv.clone();
                    if let Some(m) = c.as_object_mut() {
                        m.insert("id".into(), Value::String(k.clone()));
                    }
                    c
                } else {
                    mv.clone()
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn model_id(m: &Value) -> Option<&str> {
    m.get("id").and_then(Value::as_str)
}

fn model_ids(p: &Value) -> std::collections::HashSet<String> {
    model_values(p)
        .iter()
        .filter_map(|m| model_id(m).map(|s| s.to_ascii_lowercase()))
        .collect()
}

/// Append `extra` models to a provider's `models`, preserving its array or
/// object shape.
fn append_models(provider: &mut Value, extra: Vec<Value>) {
    let Some(obj) = provider.as_object_mut() else {
        return;
    };
    match obj.get_mut("models") {
        Some(m) if m.is_array() => {
            if let Some(a) = m.as_array_mut() {
                a.extend(extra);
            }
        }
        Some(m) if m.is_object() => {
            if let Some(o) = m.as_object_mut() {
                for mv in extra {
                    if let Some(id) = mv.get("id").and_then(Value::as_str) {
                        o.insert(id.to_string(), mv);
                    }
                }
            }
        }
        _ => {
            obj.insert("models".into(), Value::Array(extra));
        }
    }
}

/// Copy the first present key in `keys` from `src` into `dst` when `dst` does
/// not already define it (local wins).
fn fill_if_absent(dst: &mut Value, src: &Value, keys: &[&str]) {
    let dst_has = keys.iter().any(|k| dst.get(k).is_some_and(|v| !v.is_null()));
    if dst_has {
        return;
    }
    for k in keys {
        if let Some(v) = src.get(k)
            && !v.is_null()
        {
            if let Some(o) = dst.as_object_mut() {
                o.insert((*k).to_string(), v.clone());
            }
            return;
        }
    }
}

/// Replace the provider list in `store` with `merged`, preserving whether the
/// store used an object-keyed-by-id map or a plain array.
fn set_provider_entries(store: &mut Value, merged: Vec<Value>) {
    let was_object = store
        .get("providers")
        .map(|p| p.is_object())
        .unwrap_or(store.is_object());
    if let Some(obj) = store.as_object_mut() {
        if was_object {
            let mut map = serde_json::Map::new();
            for p in merged {
                let id = provider_id(&p).unwrap_or("provider").to_string();
                map.insert(id, p);
            }
            obj.insert("providers".into(), Value::Object(map));
        } else {
            obj.insert("providers".into(), Value::Array(merged));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Minimal scratch directory: `$PIR_TEST_TMP/<name>-<n>`, removed on drop.
    /// A hand-rolled `tempfile` keeps the binary's dependency list unchanged
    /// (this crate ships no dev-dependencies on purpose).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir()
                .join(format!("pir-embed-test-{}-{tag}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Scratch(base)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ent(name: &str, data: &str) -> Entry {
        Entry {
            name: name.to_string(),
            data: data.as_bytes().to_vec(),
        }
    }

    /// Frame `entries` onto a file in a fresh scratch dir that starts with
    /// `prefix` bytes (a stand-in for the executable image) and return both.
    /// Mirrors exactly what [`write_embedded`] appends.
    fn framed_temp(prefix: &[u8], entries: &[Entry]) -> (Scratch, PathBuf) {
        let dir = Scratch::new("roundtrip");
        let path = dir.join("pire");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(prefix).unwrap();
        let table = build_table(entries);
        let copied = prefix.len() as u64;
        let mut tail = Vec::new();
        tail.extend_from_slice(&table);
        tail.extend_from_slice(MAGIC);
        tail.extend_from_slice(&copied.to_le_bytes());
        f.write_all(&tail).unwrap();
        (dir, path)
    }

    #[test]
    fn roundtrip_single_entry() {
        let entries = vec![ent(ENTRY_AUTH, r#"{"openrouter":{"type":"api_key","key":"sk-1"}}"#)];
        let (_d, path) = framed_temp(b"\x7fELFpretendbinary", &entries);
        let read = read_payload(&path).expect("payload should be found");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].0, ENTRY_AUTH);
        assert_eq!(
            String::from_utf8(read[0].1.clone()).unwrap(),
            r#"{"openrouter":{"type":"api_key","key":"sk-1"}}"#
        );
    }

    #[test]
    fn roundtrip_two_entries_preserves_order() {
        let entries = vec![
            ent(ENTRY_AUTH, r#"{"a":{"type":"api_key","key":"1"}}"#),
            ent(ENTRY_STORE, r#"{"providers":[]}"#),
        ];
        let (_d, path) = framed_temp(b"binary", &entries);
        let read = read_payload(&path).unwrap();
        assert_eq!(read[0].0, ENTRY_AUTH);
        assert_eq!(read[1].0, ENTRY_STORE);
    }

    #[test]
    fn newest_trailer_wins_over_older_one() {
        // Appending a second payload (re-embedding an already-embedded pir)
        // must leave the file readable and return only the newest trailer,
        // because the newest one owns the file tail and records its own table
        // offset. This is exactly what `write_embedded` does: it copies the
        // running executable (which may itself carry an older trailer) and
        // records the bytes it copied as the new table's offset.
        let dir = Scratch::new("double");
        let path = dir.join("pire");
        let exe = b"fake executable image";
        fs::write(&path, exe).unwrap();
        for payload in [r#"{"old":1}"#, r#"{"new":1}"#] {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            let copied = f.metadata().unwrap().len();
            let table = build_table(&[ent(ENTRY_STORE, payload)]);
            f.write_all(&table).unwrap();
            f.write_all(MAGIC).unwrap();
            f.write_all(&copied.to_le_bytes()).unwrap();
        }
        let read = read_payload(&path).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(String::from_utf8(read[0].1.clone()).unwrap(), r#"{"new":1}"#);
    }

    #[test]
    fn no_payload_reads_as_none() {
        let dir = Scratch::new("plain");
        let path = dir.join("plain");
        fs::write(&path, b"just a normal binary, no trailer").unwrap();
        assert!(read_payload(&path).is_none());
    }

    #[test]
    fn truncated_or_corrupt_tail_reads_as_none() {
        let dir = Scratch::new("corrupt");
        let path = dir.join("short");
        fs::write(&path, b"tiny").unwrap();
        assert!(read_payload(&path).is_none());

        // A file whose tail is the right size but with a bad magic.
        let path2 = dir.join("badmagic");
        let mut f = fs::File::create(&path2).unwrap();
        f.write_all(b"payload-bytes-here").unwrap();
        f.write_all(b"NOTPIREM").unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap();
        f.flush().unwrap();
        assert!(read_payload(&path2).is_none());
    }

    #[test]
    fn absurd_entry_count_reads_as_none() {
        // A corrupt length must not make the reader allocate wildly.
        let dir = Scratch::new("count");
        let path = dir.join("pire");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&(u64::MAX).to_le_bytes()).unwrap();
        f.write_all(MAGIC).unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap();
        f.flush().unwrap();
        assert!(read_payload(&path).is_none());
    }

    #[test]
    fn entries_from_config_reads_both_documents() {
        let dir = Scratch::new("entries");
        fs::write(dir.join(ENTRY_AUTH), r#"{"x":{"type":"api_key","key":"k"}}"#).unwrap();
        fs::write(dir.join(ENTRY_STORE), r#"{"providers":[]}"#).unwrap();
        let entries = entries_from_config(&dir.0);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![ENTRY_AUTH, ENTRY_STORE]);
    }

    #[test]
    fn entries_from_config_skips_missing_files() {
        let dir = Scratch::new("partial");
        fs::write(dir.join(ENTRY_AUTH), "{}").unwrap();
        let entries = entries_from_config(&dir.0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, ENTRY_AUTH);
    }

    // --- merge semantics: local wins, embedded supplements ---

    #[test]
    fn merge_store_embedded_supplements_missing_provider() {
        let local = json!({"providers": [
            {"id": "localonly", "baseUrl": "http://l", "models": [{"id": "m1"}]}
        ]});
        let embedded = json!({"providers": [
            {"id": "localonly", "baseUrl": "http://e", "models": [{"id": "m1"}, {"id": "m2"}]},
            {"id": "embonly", "baseUrl": "http://eo", "models": [{"id": "e1"}]}
        ]});
        let merged = merge_store(&local, &embedded);
        let provs = merged.get("providers").and_then(Value::as_array).unwrap();
        // Both providers present.
        let ids: Vec<&str> = provs.iter().filter_map(provider_id).collect();
        assert!(ids.contains(&"localonly"));
        assert!(ids.contains(&"embonly"));
        // localonly: local baseUrl wins, model union adds m2.
        let lo = provs.iter().find(|p| provider_id(p) == Some("localonly")).unwrap();
        assert_eq!(lo.get("baseUrl").unwrap(), "http://l");
        let lo_models = model_values(lo);
        let models: Vec<&str> = lo_models.iter().filter_map(|m| model_id(m)).collect();
        assert!(models.contains(&"m1"));
        assert!(models.contains(&"m2"));
    }

    #[test]
    fn merge_store_fills_missing_key_from_embedded() {
        let local = json!({"providers": [
            {"id": "p", "models": [{"id": "m1"}]}   // no apiKey
        ]});
        let embedded = json!({"providers": [
            {"id": "p", "apiKey": "sk-embedded", "baseUrl": "http://e", "models": [{"id": "m1"}]}
        ]});
        let merged = merge_store(&local, &embedded);
        let provs = merged.get("providers").and_then(Value::as_array).unwrap();
        let p = &provs[0];
        // local had no key/url, so the embedded ones fill in.
        assert_eq!(p.get("apiKey").unwrap(), "sk-embedded");
        assert_eq!(p.get("baseUrl").unwrap(), "http://e");
    }

    #[test]
    fn merge_store_local_wins_over_embedded_key() {
        let local = json!({"providers": [
            {"id": "p", "apiKey": "sk-local", "models": [{"id": "m1"}]}
        ]});
        let embedded = json!({"providers": [
            {"id": "p", "apiKey": "sk-embedded", "models": [{"id": "m1"}]}
        ]});
        let merged = merge_store(&local, &embedded);
        let provs = merged.get("providers").and_then(Value::as_array).unwrap();
        assert_eq!(provs[0].get("apiKey").unwrap(), "sk-local");
    }

    #[test]
    fn merge_store_empty_local_uses_embedded() {
        let local = json!({"providers": []});
        let embedded = json!({"providers": [
            {"id": "e", "models": [{"id": "m"}]}
        ]});
        let merged = merge_store(&local, &embedded);
        let ids: Vec<&str> = merged
            .get("providers")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(provider_id)
            .collect();
        assert_eq!(ids, vec!["e"]);
    }

    #[test]
    fn merge_store_empty_embedded_returns_local() {
        let local = json!({"providers": [
            {"id": "p", "models": [{"id": "m"}]}
        ]});
        let merged = merge_store(&local, &json!({"providers": []}));
        let ids: Vec<&str> = merged
            .get("providers")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(provider_id)
            .collect();
        assert_eq!(ids, vec!["p"]);
    }

    #[test]
    fn merge_store_handles_object_keyed_providers() {
        let local = json!({"providers": {
            "p": {"baseUrl": "http://l", "models": [{"id": "m1"}]}
        }});
        let embedded = json!({"providers": {
            "p": {"apiKey": "sk-e", "models": [{"id": "m1"}, {"id": "m2"}]},
            "q": {"baseUrl": "http://q", "models": [{"id": "qm"}]}
        }});
        let merged = merge_store(&local, &embedded);
        let provs = merged.get("providers").and_then(Value::as_object).unwrap();
        // Both keys present in the object shape.
        assert!(provs.contains_key("p"));
        assert!(provs.contains_key("q"));
        // p gained the embedded key + model.
        let p = &provs["p"];
        assert_eq!(p.get("baseUrl").unwrap(), "http://l"); // local wins
        assert_eq!(p.get("apiKey").unwrap(), "sk-e"); // filled
        let p_models = model_values(p);
        let models: Vec<&str> = p_models.iter().filter_map(|m| model_id(m)).collect();
        assert!(models.contains(&"m2"));
    }

    // --- auth merge ---

    #[test]
    fn merge_auth_local_wins_embedded_supplements() {
        let local = json!({"shared": {"type": "api_key", "key": "local"}});
        let embedded = json!({
            "shared": {"type": "api_key", "key": "embedded"},
            "extra":  {"type": "api_key", "key": "ekey"}
        });
        let merged = merge_auth(&local, &embedded);
        assert_eq!(merged.get("shared").unwrap().get("key").unwrap(), "local");
        assert_eq!(merged.get("extra").unwrap().get("key").unwrap(), "ekey");
    }

    #[test]
    fn merge_auth_empty_local_key_is_filled() {
        let local = json!({"shared": {"type": "api_key", "key": ""}});
        let embedded = json!({"shared": {"type": "api_key", "key": "embedded"}});
        let merged = merge_auth(&local, &embedded);
        assert_eq!(merged.get("shared").unwrap().get("key").unwrap(), "embedded");
    }
}
