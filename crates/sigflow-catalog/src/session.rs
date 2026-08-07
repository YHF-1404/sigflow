//! Session layer: the explicitly declared experiment unit.
//!
//! A session is begun and ended by the operator (`sigflow-cli data session
//! begin/end`), never inferred from graph lifetime — swapping a pen without
//! restarting the graph must start a new session, and a crash-restart must
//! not split one. The mechanism:
//!
//! - `begin` writes the session manifest into the records tree FIRST
//!   (`records/<shard>/<ulid>/session.json` — a record without artifacts),
//!   THEN atomically renames the pointer `_session/current.json` into place.
//!   A crash between the two leaves an orphan session manifest (harmless,
//!   indexed with zero records) — never a pointer to nothing.
//! - The sink reads the pointer per record flush (µs-scale, never cached),
//!   so begin/end take effect on the very next capture.
//! - `end` removes the pointer. `begin` over an active session implicitly
//!   ends it (forgetting `end` costs only "boundary drawn at the next
//!   begin", never mixed attribution).
//!
//! Core safety rule (user-ratified): **every failure path degrades to
//! `session = null` (repairable via the annotation layer), never to a wrong
//! value (invisible, unrepairable)**. The sink-facing [`read_active_session`]
//! therefore refuses the pointer — with a reason it can surface — when:
//!
//! - the pointer is unreadable, unparseable, or version-refused;
//! - the session manifest it names is missing or unreadable (the
//!   manifest-before-pointer discipline was broken);
//! - the pointer is older than [`MAX_POINTER_AGE`] (a forgotten `end` must
//!   not attribute tomorrow's captures to yesterday's experiment);
//! - the manifest declares a graph and the sink runs in a different one.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::atomic::{fsync_dir, write_atomic};
use crate::id::is_valid_ulid;
use crate::layout::DataRoot;
use crate::manifest::{parse_format_version, SessionManifest, FORMAT_MAJOR, FORMAT_VERSION};
use crate::CatalogError;

/// A pointer older than this degrades to `session = null`: ~12 h covers any
/// realistic recording day, while a forgotten `end` stops mis-attributing
/// captures by the next morning.
pub const MAX_POINTER_AGE: Duration = Duration::from_secs(12 * 60 * 60);

/// On-disk shape of `_session/current.json`. Machine-local (never synced);
/// the session manifest in the records tree is the truth this points at.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionPointer {
    pub format_version: String,
    /// ULID of the active session (names `records/<shard>/<ulid>/session.json`).
    pub session_id: String,
    /// Snapshot of the manifest's `started_utc`, for the staleness check.
    pub begun_utc: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What the sink learns from one pointer read. `session_id = None` is a
/// legitimate state (recording outside any session); `degraded` is set only
/// when a pointer existed but was refused — the operator should hear why.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionReading {
    pub session_id: Option<String>,
    pub degraded: Option<String>,
}

impl SessionReading {
    fn null() -> Self {
        SessionReading { session_id: None, degraded: None }
    }

    fn refused(reason: impl Into<String>) -> Self {
        SessionReading { session_id: None, degraded: Some(reason.into()) }
    }
}

/// Begin a session: write its manifest into the records tree, then point
/// `_session/current.json` at it (atomic rename — readers see the old
/// pointer or the new one, never a torn file). Returns the pointer that was
/// active before, if any (begin implicitly ends it).
pub fn begin_session(
    root: &DataRoot,
    manifest: &SessionManifest,
) -> Result<Option<SessionPointer>, CatalogError> {
    if !is_valid_ulid(&manifest.id) {
        return Err(CatalogError::InvalidUlid(manifest.id.clone()));
    }
    let previous = read_pointer_lenient(root);

    // Manifest first: a pointer must never name a session that does not
    // exist in the records tree.
    write_atomic(&root.session_manifest(&manifest.id)?, &manifest.to_json_vec()?)?;

    let pointer = SessionPointer {
        format_version: FORMAT_VERSION.to_string(),
        session_id: manifest.id.clone(),
        begun_utc: manifest.started_utc.clone(),
        extra: Map::new(),
    };
    let mut bytes = serde_json::to_vec_pretty(&pointer)?;
    bytes.push(b'\n');
    write_atomic(&root.session_pointer(), &bytes)?;
    Ok(previous)
}

/// End the active session: remove the pointer. Returns what was active (if
/// the pointer existed and parsed). Idempotent — ending nothing is `None`,
/// not an error.
pub fn end_session(root: &DataRoot) -> Result<Option<SessionPointer>, CatalogError> {
    let previous = read_pointer_lenient(root);
    let path = root.session_pointer();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                fsync_dir(parent);
            }
            Ok(previous)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Best-effort pointer read for begin/end bookkeeping (an unparseable old
/// pointer must not block a new begin — it is about to be replaced anyway).
fn read_pointer_lenient(root: &DataRoot) -> Option<SessionPointer> {
    let bytes = std::fs::read(root.session_pointer()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The sink-facing read, run once per record flush against `expected_graph`
/// (the graph the sink runs in; `None` skips the graph check). Never errors:
/// every failure path is a refusal carrying its reason.
pub fn read_active_session(root: &DataRoot, expected_graph: Option<&str>) -> SessionReading {
    read_active_session_at(root, expected_graph, SystemTime::now(), MAX_POINTER_AGE)
}

/// Testable core of [`read_active_session`] (explicit clock and threshold).
pub fn read_active_session_at(
    root: &DataRoot,
    expected_graph: Option<&str>,
    now: SystemTime,
    max_age: Duration,
) -> SessionReading {
    let bytes = match std::fs::read(root.session_pointer()) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SessionReading::null(),
        Err(e) => return SessionReading::refused(format!("session pointer unreadable: {e}")),
    };
    let pointer: SessionPointer = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => return SessionReading::refused(format!("session pointer unparseable: {e}")),
    };
    match parse_format_version(&pointer.format_version) {
        Some((maj, _)) if maj == FORMAT_MAJOR => {}
        _ => {
            return SessionReading::refused(format!(
                "session pointer format_version '{}' refused (this tool reads major {})",
                pointer.format_version, FORMAT_MAJOR
            ))
        }
    }
    if !is_valid_ulid(&pointer.session_id) {
        return SessionReading::refused(format!(
            "session pointer names invalid ULID '{}'",
            pointer.session_id
        ));
    }

    // The pointed-at manifest must exist and parse: the pointer is only a
    // pointer, the records tree holds the truth.
    let manifest_path = match root.session_manifest(&pointer.session_id) {
        Ok(p) => p,
        Err(e) => return SessionReading::refused(e.to_string()),
    };
    let manifest = match std::fs::read(&manifest_path) {
        Ok(b) => match SessionManifest::from_slice(&b) {
            Ok(m) => m,
            Err(e) => {
                return SessionReading::refused(format!(
                    "session manifest {} unreadable: {e}",
                    manifest_path.display()
                ))
            }
        },
        Err(e) => {
            return SessionReading::refused(format!(
                "session manifest {} missing ({e}) — pointer without manifest",
                manifest_path.display()
            ))
        }
    };
    // The manifest must BE the session the pointer names — a copied/synced
    // file with another id would otherwise produce a non-null reading whose
    // attributes belong to a different session (a wrong value, the one thing
    // every failure path must never produce).
    if manifest.id != pointer.session_id {
        return SessionReading::refused(format!(
            "session manifest {} carries id '{}' (≠ pointer) — corrupted session dir",
            manifest_path.display(),
            manifest.id
        ));
    }

    // Staleness: a forgotten `end` must stop attributing captures by ~12 h.
    let begun = match humantime::parse_rfc3339(&pointer.begun_utc) {
        Ok(t) => t,
        Err(e) => {
            return SessionReading::refused(format!(
                "session pointer begun_utc '{}' unparseable: {e}",
                pointer.begun_utc
            ))
        }
    };
    // A begun_utc in the future (wall clock stepped back) is not stale.
    let age = now.duration_since(begun).unwrap_or(Duration::ZERO);
    if age > max_age {
        return SessionReading::refused(format!(
            "session pointer is {:.1} h old (> {:.0} h) — forgotten `data session end`?",
            age.as_secs_f64() / 3600.0,
            max_age.as_secs_f64() / 3600.0
        ));
    }

    // Graph mismatch: the session was declared for another topology.
    if let (Some(expected), Some(declared)) = (expected_graph, manifest.graph.as_deref()) {
        if expected != declared {
            return SessionReading::refused(format!(
                "session {} was begun for graph '{declared}', this sink runs in '{expected}'",
                pointer.session_id
            ));
        }
    }

    SessionReading { session_id: Some(pointer.session_id), degraded: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utc_now_rfc3339;

    const U1: &str = "01JXAB3C4D5E6F7G8H9JKMNPQR";
    const U2: &str = "01JXAB3C4D5E6F7G8H9JKMNPQS";

    fn session(id: &str, graph: Option<&str>) -> SessionManifest {
        SessionManifest {
            format_version: FORMAT_VERSION.to_string(),
            id: id.to_string(),
            started_utc: utc_now_rfc3339(),
            graph: graph.map(str::to_string),
            host: "test".into(),
            attrs: serde_json::from_str(r#"{"subject": "alice"}"#).unwrap(),
            sink_attrs: Map::new(),
            extra: Map::new(),
        }
    }

    #[test]
    fn begin_then_read_round_trips() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        assert_eq!(begin_session(&root, &session(U1, None)).unwrap(), None);
        assert!(root.session_manifest(U1).unwrap().exists());

        let r = read_active_session(&root, None);
        assert_eq!(r.session_id.as_deref(), Some(U1));
        assert_eq!(r.degraded, None);
    }

    #[test]
    fn begin_implicitly_ends_the_previous_session() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        begin_session(&root, &session(U1, None)).unwrap();
        let prev = begin_session(&root, &session(U2, None)).unwrap().unwrap();
        assert_eq!(prev.session_id, U1);
        assert_eq!(read_active_session(&root, None).session_id.as_deref(), Some(U2));
        // The replaced session's manifest stays in the records tree (truth).
        assert!(root.session_manifest(U1).unwrap().exists());
    }

    #[test]
    fn end_removes_the_pointer_and_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        begin_session(&root, &session(U1, None)).unwrap();
        let ended = end_session(&root).unwrap().unwrap();
        assert_eq!(ended.session_id, U1);
        assert_eq!(read_active_session(&root, None), SessionReading::null());
        assert_eq!(end_session(&root).unwrap(), None);
    }

    #[test]
    fn no_pointer_is_null_without_complaint() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        assert_eq!(read_active_session(&root, None), SessionReading::null());
    }

    #[test]
    fn manifest_with_foreign_id_is_refused() {
        // A session.json copied over another's (sync accident, fat-finger)
        // must refuse, not serve session A under session B's attributes.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        begin_session(&root, &session(U1, None)).unwrap();
        begin_session(&root, &session(U2, None)).unwrap();
        std::fs::copy(
            root.session_manifest(U2).unwrap(),
            root.session_manifest(U1).unwrap(),
        )
        .unwrap();
        // Point back at U1, whose manifest now claims to be U2.
        let ptr = format!(
            r#"{{"format_version":"1.0","session_id":"{U1}","begun_utc":"{}"}}"#,
            utc_now_rfc3339()
        );
        std::fs::write(root.session_pointer(), ptr).unwrap();

        let r = read_active_session(&root, None);
        assert_eq!(r.session_id, None);
        assert!(r.degraded.unwrap().contains("≠ pointer"));
    }

    #[test]
    fn pointer_without_manifest_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        begin_session(&root, &session(U1, None)).unwrap();
        std::fs::remove_file(root.session_manifest(U1).unwrap()).unwrap();
        let r = read_active_session(&root, None);
        assert_eq!(r.session_id, None);
        assert!(r.degraded.unwrap().contains("missing"));
    }

    #[test]
    fn corrupt_or_future_version_pointer_is_refused_not_panicked() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        std::fs::create_dir_all(root.session_pointer().parent().unwrap()).unwrap();

        std::fs::write(root.session_pointer(), b"{ not json").unwrap();
        let r = read_active_session(&root, None);
        assert_eq!(r.session_id, None);
        assert!(r.degraded.unwrap().contains("unparseable"));

        let v2 = format!(
            r#"{{"format_version":"2.0","session_id":"{U1}","begun_utc":"2026-06-11T00:00:00.000Z"}}"#
        );
        std::fs::write(root.session_pointer(), v2).unwrap();
        let r = read_active_session(&root, None);
        assert_eq!(r.session_id, None);
        assert!(r.degraded.unwrap().contains("format_version"));
    }

    #[test]
    fn stale_pointer_is_refused_future_begun_is_not() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        begin_session(&root, &session(U1, None)).unwrap();

        let now = SystemTime::now();
        let r = read_active_session_at(&root, None, now + MAX_POINTER_AGE * 2, MAX_POINTER_AGE);
        assert_eq!(r.session_id, None);
        assert!(r.degraded.unwrap().contains("forgotten"), "stale must explain itself");

        // Wall clock stepped back below begun_utc: not stale.
        let r = read_active_session_at(
            &root,
            None,
            now - Duration::from_secs(3600),
            MAX_POINTER_AGE,
        );
        assert_eq!(r.session_id.as_deref(), Some(U1));
    }

    #[test]
    fn graph_mismatch_is_refused_match_or_undeclared_pass() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        begin_session(&root, &session(U1, Some("vofa-dual-pen"))).unwrap();

        let r = read_active_session(&root, Some("other-graph"));
        assert_eq!(r.session_id, None);
        assert!(r.degraded.unwrap().contains("graph"));

        assert_eq!(
            read_active_session(&root, Some("vofa-dual-pen")).session_id.as_deref(),
            Some(U1)
        );
        // Sink without a graph param: no constraint to check.
        assert_eq!(read_active_session(&root, None).session_id.as_deref(), Some(U1));

        // Session without a declared graph: no constraint either.
        begin_session(&root, &session(U2, None)).unwrap();
        assert_eq!(
            read_active_session(&root, Some("other-graph")).session_id.as_deref(),
            Some(U2)
        );
    }

    #[test]
    fn begin_rejects_invalid_ulid() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let err = begin_session(&root, &session("not-a-ulid", None)).unwrap_err();
        assert!(matches!(err, CatalogError::InvalidUlid(_)));
    }
}
