//! state.json persistence — atomic writes, merge, purge, ack mirror.
//!
//! The only place that touches state.json bytes. Handlers call these; they
//! never open the file themselves. tmp + rename = atomic: readers (tmux bar,
//! CLI) see either the old or the new file, never a mix.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use muxmesh_core::{now_unix, HostState, StateDoc};

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// hub.log sits next to state.json (same directory, derived path).
pub fn log_path_for(state_path: &Path) -> PathBuf {
    state_path
        .parent()
        .map(|d| d.join("hub.log"))
        .unwrap_or_else(|| PathBuf::from("hub.log"))
}

/// Read state.json as the typed contract doc. Missing/corrupt -> empty.
#[allow(dead_code)] // used by purge_and_write; kept public for tests/CLI parity
pub fn read_doc(path: &Path) -> StateDoc {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| StateDoc::from_json_str(&s).ok())
        .unwrap_or_default()
}

/// tmp + rename = atomic. tmp name is unique per write (pid + counter) so
/// concurrent writers can never splice into the same scratch file.
pub fn write_atomic(path: &Path, doc: &StateDoc) -> std::io::Result<()> {
    let json = doc.to_json_pretty().map_err(std::io::Error::other)?;
    write_bytes_atomic(path, json.as_bytes())
}

pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// tmp + rename, mode 0600 (tokens.json holds digests, still keep it tight).
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_bytes_atomic(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Merge one HostState into state.json (caller holds the write lock).
/// Returns the debug trace ("armed=.. sessions=[..]"). Raw JSON doc (not
/// StateDoc) so the out-of-band last_cmd ack mirror survives the merge.
pub fn merge_state(path: &Path, host_key: &str, st: &HostState) -> std::io::Result<String> {
    let mut raw: serde_json::Value = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hosts": {} }));
    if !raw.get("hosts").is_some_and(|h| h.is_object()) {
        raw["hosts"] = serde_json::json!({});
    }
    let hosts = raw.get_mut("hosts").unwrap();
    // trace BEFORE the move into the map (armed flag + session/window names),
    // so an operator can follow F10's chain (the driver reads THIS state).
    let trace = format!(
        "armed={} sessions=[{}]",
        st.armed,
        st.sessions
            .iter()
            .map(|s| format!("{}:{}w/active={}", s.name, s.windows, s.active_window))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut entry = serde_json::to_value(st).map_err(std::io::Error::other)?;
    // carry the ack mirror across the merge (the agent payload has no last_cmd)
    if let Some(prev) = hosts.get(host_key).and_then(|h| h.get("last_cmd")) {
        entry["last_cmd"] = prev.clone();
    }
    hosts[host_key] = entry;
    let json = serde_json::to_string_pretty(&raw).map_err(std::io::Error::other)?;
    write_bytes_atomic(path, json.as_bytes())?;
    Ok(trace)
}

/// Mirror a command ack into state.json under hosts.<host>.last_cmd so the
/// driver can read it back via GET /state.
pub fn record_ack(path: &Path, host: &str, ack: &muxmesh_core::Ack) {
    let mut raw: serde_json::Value = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hosts": {} }));
    if !raw.get("hosts").is_some_and(|h| h.is_object()) {
        raw["hosts"] = serde_json::json!({});
    }
    // the ack may arrive BEFORE the agent's first state report (test flow and
    // cold start alike) — create the host entry rather than drop the ack.
    if !raw["hosts"].get(host).is_some_and(|h| h.is_object()) {
        raw["hosts"][host] = serde_json::json!({});
    }
    raw["hosts"][host]["last_cmd"] = serde_json::json!({
        "id": ack.id,
        "ok": ack.ok,
        "err": ack.err,
        "at": now_unix(),
    });
    if let Ok(json) = serde_json::to_string_pretty(&raw) {
        if let Err(e) = write_bytes_atomic(path, json.as_bytes()) {
            tracing::warn!("last_cmd write {host}: {e}");
        }
    }
}

/// Remove expired entries and rewrite the file (called at startup).
pub fn purge_and_write(path: &Path, ttl: u64) -> std::io::Result<()> {
    let mut doc = read_doc(path);
    let now = now_unix();
    let before = doc.hosts.len();
    doc.hosts.retain(|_, h| h.is_fresh(now, ttl));
    if doc.hosts.len() != before {
        write_atomic(path, &doc)?;
    }
    Ok(())
}
