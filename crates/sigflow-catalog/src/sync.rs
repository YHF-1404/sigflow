//! `_sync/synced.json` — the one designed cross-machine backflow file.
//!
//! Direction (the only thing that flows capture ← analysis): the analysis
//! machine hash-verifies the records it pulled (via `reindex`) and records
//! each verified ULID here, then pushes this single file back to the capture
//! machine. The capture machine's [`crate::gc`] reads it as the proof that a
//! verified copy exists elsewhere before it reclaims card space.
//!
//! This does not break the writer-separation invariant: records stay
//! capture-owned, annotations stay analysis-owned, and this file carries no
//! truth — only an acknowledgement a re-verify can rebuild from scratch. The
//! analysis machine writes, the capture machine reads.
//!
//! RECONCILED to current verification, not merely appended: each ack run sets
//! the verified set to exactly what the analysis machine verifies *now* —
//! newly-verified ULIDs are added (keeping their first-verified timestamp once
//! set), and a ULID that no longer verifies (re-verification failed, e.g.
//! `reindex --full` caught bit rot, or the analysis copy was removed) is
//! RETRACTED. Retraction is the safe direction for the capture machine's gc:
//! it can only un-collect (protect a copy), never mis-collect. An append-only
//! "acks only grow" set was the original design but is unsafe — it keeps
//! vouching for a copy that has since degraded, letting gc reclaim the last
//! good copy.
//!
//! Schema: `{ "format_version", "host"?, "verified": { ulid: utc_ts } }`.
//! `host` is the writer (analysis) machine — `gc` uses it as a tripwire so it
//! never runs against an ack it wrote itself (which would record this
//! machine's own copies, not an independent backup).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::atomic::write_atomic;
use crate::layout::DataRoot;
use crate::manifest::parse_format_version;
use crate::CatalogError;

/// Version written by this crate.
pub const SYNC_FORMAT_VERSION: &str = "1.0";
/// Major version this crate can read (mirrors the manifest gate: minor
/// additions read by skipping unknowns, an unknown major is refused).
pub const SYNC_FORMAT_MAJOR: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncAck {
    pub format_version: String,
    /// Hostname of the analysis machine that wrote this ack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// ULID → first-verified UTC timestamp (RFC 3339). Sorted (BTreeMap) for
    /// a stable on-disk diff.
    #[serde(default)]
    pub verified: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for SyncAck {
    fn default() -> Self {
        SyncAck::new()
    }
}

impl SyncAck {
    pub fn new() -> Self {
        SyncAck {
            format_version: SYNC_FORMAT_VERSION.to_string(),
            host: None,
            verified: BTreeMap::new(),
            extra: Map::new(),
        }
    }

    /// Deserialize, refusing an unknown major version (a newer major may have
    /// restructured the file — that must report `Version` ("upgrade the
    /// tool"), never `Json` ("damage"), and must never be silently downgraded
    /// by an older tool's write).
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CatalogError> {
        gate_version(bytes)?;
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn to_json_vec(&self) -> Result<Vec<u8>, CatalogError> {
        let mut v = serde_json::to_vec_pretty(self)?;
        v.push(b'\n');
        Ok(v)
    }

    /// Reconcile the verified set to exactly `current` — the ULIDs the
    /// analysis machine verifies right now. Newly-verified ULIDs are added
    /// (stamped `now`); already-acked ones keep their first-verified ts; and
    /// ULIDs no longer in `current` are RETRACTED. Returns `(added,
    /// retracted)`. Retraction is gc-safe (it can only protect a copy, never
    /// reclaim one wrongly), so a record that stops verifying stops being
    /// vouched for — closing the "stale ack vouches for a degraded copy" hole.
    pub fn reconcile_verified<'a>(
        &mut self,
        current: impl IntoIterator<Item = &'a str>,
        now: &str,
    ) -> (usize, usize) {
        let mut next = BTreeMap::new();
        let mut added = 0;
        for u in current {
            match self.verified.get(u) {
                Some(ts) => {
                    next.insert(u.to_string(), ts.clone());
                }
                None => {
                    next.insert(u.to_string(), now.to_string());
                    added += 1;
                }
            }
        }
        let retracted = self.verified.keys().filter(|k| !next.contains_key(k.as_str())).count();
        self.verified = next;
        (added, retracted)
    }
}

fn gate_version(bytes: &[u8]) -> Result<(), CatalogError> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        format_version: String,
    }
    let p: Probe = serde_json::from_slice(bytes)?;
    match parse_format_version(&p.format_version) {
        Some((maj, _)) if maj == SYNC_FORMAT_MAJOR => Ok(()),
        _ => Err(CatalogError::Version { found: p.format_version }),
    }
}

/// Read `_sync/synced.json`. `Ok(None)` when the file simply does not exist
/// (nothing acked yet); `Err` when it exists but is damaged or
/// version-refused — the caller (gc) must refuse rather than treat a corrupt
/// ack as "nothing verified" (which would silently collect nothing) OR risk
/// acting on a misread.
pub fn read_sync_ack(root: &DataRoot) -> Result<Option<SyncAck>, CatalogError> {
    match std::fs::read(root.sync_ack()) {
        Ok(b) => Ok(Some(SyncAck::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write `_sync/synced.json` atomically (write-tmp + rename + dir fsync — a
/// crash leaves the old ack or the new one, never a torn file). The analysis
/// machine is the sole writer; no lock is needed.
pub fn write_sync_ack(root: &DataRoot, ack: &SyncAck) -> Result<(), CatalogError> {
    write_atomic(&root.sync_ack(), &ack.to_json_vec()?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const U1: &str = "01JXAB3C4D5E6F7G8H9JKMNPQR";
    const U2: &str = "01JXAB3C4D5E6F7G8H9JKMNPQS";

    #[test]
    fn roundtrips_and_preserves_unknown_fields() {
        let mut a = SyncAck::new();
        a.host = Some("mac-studio".into());
        a.verified.insert(U1.into(), "2026-06-13T00:00:00.000Z".into());
        // A newer minor version adding a top-level field must survive.
        let mut v: Value = serde_json::from_slice(&a.to_json_vec().unwrap()).unwrap();
        v["future_field"] = serde_json::json!({"x": 1});
        let back = SyncAck::from_slice(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(back.host.as_deref(), Some("mac-studio"));
        assert_eq!(back.verified[U1], "2026-06-13T00:00:00.000Z");
        let reser: Value = serde_json::from_slice(&back.to_json_vec().unwrap()).unwrap();
        assert_eq!(reser["future_field"]["x"], 1, "unknown field preserved");
    }

    #[test]
    fn version_gate_refuses_unknown_major() {
        let mut v: Value = serde_json::from_slice(&SyncAck::new().to_json_vec().unwrap()).unwrap();
        v["format_version"] = "1.9".into();
        assert!(SyncAck::from_slice(&serde_json::to_vec(&v).unwrap()).is_ok(), "newer minor ok");
        v["format_version"] = "2.0".into();
        assert!(matches!(
            SyncAck::from_slice(&serde_json::to_vec(&v).unwrap()).unwrap_err(),
            CatalogError::Version { .. }
        ));
        v["format_version"] = "garbage".into();
        assert!(matches!(
            SyncAck::from_slice(&serde_json::to_vec(&v).unwrap()).unwrap_err(),
            CatalogError::Version { .. }
        ));
    }

    #[test]
    fn reconcile_adds_keeps_ts_and_retracts() {
        let mut a = SyncAck::new();
        assert_eq!(a.reconcile_verified([U1, U2], "t1"), (2, 0));
        // U1 re-verifies (sticky ts, not counted new); U2 drops out (no longer
        // verified → retracted); U2's slot is freed so gc protects its copy.
        assert_eq!(a.reconcile_verified([U1], "t2"), (0, 1));
        assert_eq!(a.verified[U1], "t1", "first-verified ts is sticky");
        assert!(!a.verified.contains_key(U2), "un-verified ULID retracted");
        assert_eq!(a.verified.len(), 1);
        // A reconcile against the empty set retracts everything (safe: gc then
        // collects nothing) — never an error, never silent corruption.
        assert_eq!(a.reconcile_verified(std::iter::empty(), "t3"), (0, 1));
        assert!(a.verified.is_empty());
    }

    #[test]
    fn read_absent_is_none_present_damaged_is_err() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        assert!(read_sync_ack(&root).unwrap().is_none(), "absent → None");

        let mut a = SyncAck::new();
        a.verified.insert(U1.into(), "t1".into());
        write_sync_ack(&root, &a).unwrap();
        assert_eq!(read_sync_ack(&root).unwrap().unwrap().verified[U1], "t1");

        // A present-but-corrupt ack is an error, never a silent empty ack.
        std::fs::write(root.sync_ack(), b"{ not json").unwrap();
        assert!(read_sync_ack(&root).is_err(), "damaged ack must error, not read as empty");
    }
}
