//! Human-annotation files — the mutable truth layer.
//!
//! `annotations/records/<ulid>.json` and `annotations/sessions/<ulid>.json`
//! are the ONLY mutable files in the catalog and the only irreplaceable asset
//! (everything else is either immutable or recomputable). Ratified contract:
//!
//! - Every key stores a `{value, ts, writer}` triplet, never a bare value —
//!   the near-lossless hedge for a future migration to an append-only log.
//! - Unknown fields are preserved on round-trip; an unknown `format_version`
//!   major is refused (an old tool must never silently drop what a newer one
//!   wrote).
//! - `rev` is the optimistic-concurrency counter the S7 writer library CAS-es
//!   on (flock + rev check). Readers ignore it.
//! - The `deleted` key is the tombstone: deletion is an annotation, not a
//!   record mutation. It is kept forever (a stale backup restore must not
//!   resurrect a deleted record) and hides the record from default queries.
//!
//! All writes go through [`annotate`] — THE writer library (CLI, GUI and
//! tools alike; never hand-write these files — rename keeps a file intact,
//! not a merge correct). It serializes read-modify-write cycles with a
//! global lock on the `annotations/` tree (directory flock on Unix, an
//! `annotations/.lock` file on Windows) and offers an optional rev CAS on
//! top for callers that staged an edit against a snapshot.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::atomic::write_atomic;
use crate::journal::JournalEntry;
use crate::layout::DataRoot;
use crate::manifest::{parse_format_version, FORMAT_MAJOR, FORMAT_VERSION};
use crate::CatalogError;

/// The tombstone key (see module docs).
pub const KEY_DELETED: &str = "deleted";

/// One annotated key: the value plus when and by whom it was last written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnotValue {
    pub value: Value,
    /// RFC 3339 UTC of the last write.
    pub ts: String,
    /// Writer identity, e.g. `mac/sigflow-cli`.
    pub writer: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One annotation file (per record or per session).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnotationFile {
    pub format_version: String,
    /// Optimistic-concurrency revision, bumped by every successful write.
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub keys: std::collections::BTreeMap<String, AnnotValue>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AnnotationFile {
    pub fn new() -> Self {
        AnnotationFile {
            format_version: FORMAT_VERSION.to_string(),
            rev: 0,
            keys: Default::default(),
            extra: Map::new(),
        }
    }

    /// Deserialize, refusing an unknown major version. Mirrors the manifest
    /// gate: a probe runs before the typed parse so a restructured newer
    /// major reports `Version` ("upgrade the tool"), not `Json` ("bit rot").
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CatalogError> {
        #[derive(Deserialize)]
        struct Probe {
            #[serde(default)]
            format_version: String,
        }
        let p: Probe = serde_json::from_slice(bytes)?;
        match parse_format_version(&p.format_version) {
            Some((maj, _)) if maj == FORMAT_MAJOR => {}
            _ => return Err(CatalogError::Version { found: p.format_version }),
        }
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn to_json_vec(&self) -> Result<Vec<u8>, CatalogError> {
        let mut v = serde_json::to_vec_pretty(self)?;
        v.push(b'\n');
        Ok(v)
    }

    /// The record is tombstoned iff `deleted` is present and not `false`/
    /// `null` (so `deleted: true` and `deleted: "2026-06-11 mistake"` both
    /// hide it, while explicitly writing `false` un-hides).
    pub fn is_deleted(&self) -> bool {
        match self.keys.get(KEY_DELETED) {
            None => false,
            Some(a) => !matches!(a.value, Value::Bool(false) | Value::Null),
        }
    }
}

impl Default for AnnotationFile {
    fn default() -> Self {
        Self::new()
    }
}

/// Read an annotation file if it exists. Not-found is a normal state (most
/// records are never annotated) → `Ok(None)`. A present-but-unreadable or
/// unparsable file is an error the caller must surface — silently treating
/// damaged human annotations as absent would drop the one irreplaceable
/// layer from query results.
pub fn read_annotation_file(path: &std::path::Path) -> Result<Option<AnnotationFile>, CatalogError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(AnnotationFile::from_slice(&bytes)?))
}

/// Which entity an annotation write targets. The path and the protected-key
/// set differ; everything else (locking, CAS, merge, journal) is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotEntity<'a> {
    Record(&'a str),
    Session(&'a str),
}

impl<'a> AnnotEntity<'a> {
    pub fn ulid(&self) -> &'a str {
        match self {
            AnnotEntity::Record(u) | AnnotEntity::Session(u) => u,
        }
    }

    /// Journal/display target string, e.g. `records/<ulid>`.
    pub fn target(&self) -> String {
        match self {
            AnnotEntity::Record(u) => format!("records/{u}"),
            AnnotEntity::Session(u) => format!("sessions/{u}"),
        }
    }

    /// Keys a Set must refuse: the resolver force-overrides identity and
    /// original timestamps, so writing them would be an inert lie — the
    /// operator would believe a change that no query will ever show.
    pub fn protected_keys(&self) -> &'static [&'static str] {
        match self {
            AnnotEntity::Record(_) => crate::resolve::PROTECTED_KEYS,
            AnnotEntity::Session(_) => &["id", "started_utc"],
        }
    }

    /// The annotation file this entity's writes go to (validates the ULID —
    /// a malformed id must never become a path component).
    pub fn annotation_path(&self, root: &DataRoot) -> Result<PathBuf, CatalogError> {
        match self {
            AnnotEntity::Record(u) => root.record_annotation(u),
            AnnotEntity::Session(u) => root.session_annotation(u),
        }
    }

    /// The manifest whose existence makes an annotation non-dangling
    /// (`record.json` / `session.json` in the records tree).
    pub fn manifest_path(&self, root: &DataRoot) -> Result<PathBuf, CatalogError> {
        match self {
            AnnotEntity::Record(u) => root.record_manifest(u),
            AnnotEntity::Session(u) => root.session_manifest(u),
        }
    }
}

/// One mutation in an [`annotate`] batch. Mutations apply in order, so a Set
/// and an Unset of the same key resolve to whichever comes last.
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotMutation {
    /// Insert or replace the key. Replacement is wholesale: the previous
    /// `{value, ts, writer}` triplet — including any unknown sibling fields
    /// a newer tool attached to *that* write — describes the superseded
    /// write event and goes with it. Unknown fields of other keys and of the
    /// file itself are preserved.
    Set { key: String, value: Value },
    /// Remove the key entirely (a typo'd key, say). Un-deleting a tombstone
    /// is better done with `deleted=false`: that leaves the repair on file.
    Unset { key: String },
}

/// Outcome of a successful [`annotate`] batch.
#[derive(Debug)]
pub struct AnnotWriteOutcome {
    /// Revision the batch committed as.
    pub rev: u64,
    /// The annotation file did not exist before this write.
    pub created: bool,
    /// Unset keys that were not present (surfaced so a typo'd `--unset`
    /// does not silently do nothing).
    pub unset_missing: Vec<String>,
    /// Journal trouble, if any. The write itself committed — the journal is
    /// droppable by contract and must never gate it.
    pub journal_warning: Option<String>,
    /// File state after the write.
    pub file: AnnotationFile,
}

/// Write annotations — THE writer library. Serialization is a global flock
/// on `annotations/.lock` bracketing the read-modify-write (two processes —
/// or two handles in one — never interleave); `expected_rev` adds an
/// optimistic CAS on top for callers that staged their edit against a
/// snapshot (a GUI form, say). Pass `None` for plain
/// read-modify-write-under-lock semantics.
///
/// Refusals (each leaves the file untouched):
/// - damaged existing file (unparsable JSON): overwriting would destroy the
///   only irreplaceable layer — inspect or move it by hand first
/// - `format_version` with an unknown major: a newer tool wrote structure
///   this one would silently mangle
/// - `expected_rev` mismatch → [`CatalogError::Conflict`]
/// - a Set on a protected key (see [`AnnotEntity::protected_keys`])
/// - an empty batch or an empty key
pub fn annotate(
    root: &DataRoot,
    entity: AnnotEntity,
    mutations: &[AnnotMutation],
    writer: &str,
    expected_rev: Option<u64>,
) -> Result<AnnotWriteOutcome, CatalogError> {
    annotate_at(root, entity, mutations, writer, expected_rev, &crate::utc_now_rfc3339())
}

pub(crate) fn annotate_at(
    root: &DataRoot,
    entity: AnnotEntity,
    mutations: &[AnnotMutation],
    writer: &str,
    expected_rev: Option<u64>,
    ts: &str,
) -> Result<AnnotWriteOutcome, CatalogError> {
    if mutations.is_empty() {
        return Err(CatalogError::Invalid("empty annotation batch (nothing to write)".into()));
    }
    let protected = entity.protected_keys();
    for m in mutations {
        let key = match m {
            AnnotMutation::Set { key, .. } | AnnotMutation::Unset { key } => key.trim(),
        };
        if key.is_empty() {
            return Err(CatalogError::Invalid("empty annotation key".into()));
        }
        if matches!(m, AnnotMutation::Set { .. }) && protected.contains(&key) {
            return Err(CatalogError::Invalid(format!(
                "key '{key}' is protected (identity/original timestamps — the resolver \
                 force-overrides it, so the write would be inert)"
            )));
        }
    }
    let path = entity.annotation_path(root)?; // ULID gate

    // ---- writer lock (held to the end of the function) ----
    // Unix: flock the annotations/ directory itself, not a dedicated lock
    // file — its inode survives anything short of deleting the data it
    // guards (see `DataRoot::annotations_dir`). A read-only fd is enough;
    // flock needs no write permission.
    let lock_dir = root.annotations_dir();
    std::fs::create_dir_all(&lock_dir)?;
    #[cfg(not(windows))]
    let lock = std::fs::File::open(&lock_dir)?;
    // Windows: `File::open` cannot produce a directory handle (that needs
    // backup semantics) and `LockFileEx` does not lock directories anyway —
    // lock a `.lock` file inside the guarded tree instead. Same lifetime
    // argument: it can only vanish together with the data it guards.
    #[cfg(windows)]
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_dir.join(".lock"))?;
    lock.lock()?; // LOCK_EX / LockFileEx: blocks; released when `lock` drops

    // ---- read-modify-write under the lock ----
    let existing = match std::fs::read(&path) {
        Ok(bytes) => match AnnotationFile::from_slice(&bytes) {
            Ok(f) => Some(f),
            Err(e @ CatalogError::Version { .. }) => return Err(e),
            Err(e) => {
                return Err(CatalogError::Invalid(format!(
                    "annotation file {} is damaged ({e}) — refusing to overwrite the only \
                     irreplaceable layer; inspect or move it by hand, then retry",
                    path.display()
                )))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let created = existing.is_none();
    let mut file = existing.unwrap_or_default();
    if let Some(expected) = expected_rev {
        if file.rev != expected {
            return Err(CatalogError::Conflict { expected, found: file.rev });
        }
    }

    let mut unset_missing = Vec::new();
    for m in mutations {
        match m {
            AnnotMutation::Set { key, value } => {
                file.keys.insert(
                    key.trim().to_string(),
                    AnnotValue {
                        value: value.clone(),
                        ts: ts.to_string(),
                        writer: writer.to_string(),
                        extra: Map::new(),
                    },
                );
            }
            AnnotMutation::Unset { key } => {
                if file.keys.remove(key.trim()).is_none() {
                    unset_missing.push(key.trim().to_string());
                }
            }
        }
    }
    // checked, not wrapping: a real rev never approaches u64::MAX, but a
    // single-bit corruption could land one there, and that file still parses
    // as a valid u64 (not "damaged"). Wrapping to 0 would be a silent wrong
    // value that breaks every later `expected_rev` CAS — refuse instead.
    file.rev = file.rev.checked_add(1).ok_or_else(|| {
        CatalogError::Invalid(format!(
            "annotation file {} has rev at u64::MAX — corrupt; refusing to wrap to 0",
            path.display()
        ))
    })?;
    write_atomic(&path, &file.to_json_vec()?)?;

    // ---- journal: still under the lock, so lines stay rev-ordered ----
    let target = entity.target();
    let entries: Vec<JournalEntry> = mutations
        .iter()
        .map(|m| match m {
            AnnotMutation::Set { key, value } => JournalEntry {
                ts,
                writer,
                verb: "set",
                target: &target,
                key,
                value: Some(value),
                rev: file.rev,
            },
            AnnotMutation::Unset { key } => JournalEntry {
                ts,
                writer,
                verb: "unset",
                target: &target,
                key,
                value: None,
                rev: file.rev,
            },
        })
        .collect();
    let journal_warning = crate::journal::append(root, &entries);

    Ok(AnnotWriteOutcome { rev: file.rev, created, unset_missing, journal_warning, file })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> Vec<u8> {
        br#"{
            "format_version": "1.0",
            "rev": 3,
            "keys": {
                "grade": {"value": "good", "ts": "2026-06-11T01:00:00.000Z", "writer": "mac/cli"},
                "dt_ms_note": {"value": 12.5, "ts": "2026-06-11T01:00:00.000Z", "writer": "mac/cli", "future_field": 1}
            },
            "future_top": {"x": 1}
        }"#
        .to_vec()
    }

    #[test]
    fn roundtrip_preserves_unknown_fields() {
        let a = AnnotationFile::from_slice(&sample_json()).unwrap();
        assert_eq!(a.rev, 3);
        assert_eq!(a.keys["grade"].value, "good");
        let back: Value = serde_json::from_slice(&a.to_json_vec().unwrap()).unwrap();
        assert_eq!(back["future_top"]["x"], 1);
        assert_eq!(back["keys"]["dt_ms_note"]["future_field"], 1);
    }

    #[test]
    fn version_gate_refuses_unknown_major() {
        let err =
            AnnotationFile::from_slice(br#"{"format_version":"2.0","entries":[]}"#).unwrap_err();
        assert!(matches!(err, CatalogError::Version { .. }), "{err}");
        // Newer minor reads fine.
        assert!(AnnotationFile::from_slice(br#"{"format_version":"1.9","keys":{}}"#).is_ok());
    }

    #[test]
    fn tombstone_semantics() {
        let mut a = AnnotationFile::new();
        assert!(!a.is_deleted());
        let ent = |v: Value| AnnotValue {
            value: v,
            ts: "2026-06-11T01:00:00.000Z".into(),
            writer: "t".into(),
            extra: Map::new(),
        };
        a.keys.insert(KEY_DELETED.into(), ent(Value::Bool(true)));
        assert!(a.is_deleted());
        a.keys.insert(KEY_DELETED.into(), ent(Value::String("oops".into())));
        assert!(a.is_deleted());
        a.keys.insert(KEY_DELETED.into(), ent(Value::Bool(false)));
        assert!(!a.is_deleted(), "explicit false un-hides");
        a.keys.insert(KEY_DELETED.into(), ent(Value::Null));
        assert!(!a.is_deleted());
    }

    #[test]
    fn read_missing_is_none_damaged_is_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(read_annotation_file(&t.path().join("nope.json")).unwrap().is_none());
        let p = t.path().join("bad.json");
        std::fs::write(&p, b"{ not json").unwrap();
        assert!(read_annotation_file(&p).is_err());
    }

    // ---- write side ----

    const U: &str = "01JXAB3C4D5E6F7G8H9JKMNPQR";

    fn set(key: &str, value: Value) -> AnnotMutation {
        AnnotMutation::Set { key: key.into(), value }
    }

    fn unset(key: &str) -> AnnotMutation {
        AnnotMutation::Unset { key: key.into() }
    }

    #[test]
    fn annotate_creates_bumps_rev_and_records_provenance() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);

        let out = annotate(&root, e, &[set("grade", "good".into())], "w1", None).unwrap();
        assert!(out.created);
        assert_eq!(out.rev, 1);

        let out = annotate(&root, e, &[set("deleted", true.into())], "w2", None).unwrap();
        assert!(!out.created);
        assert_eq!(out.rev, 2);
        assert_eq!(out.file.keys["grade"].value, "good");
        assert_eq!(out.file.keys["grade"].writer, "w1");
        assert_eq!(out.file.keys["deleted"].writer, "w2");
        assert!(out.file.is_deleted());

        // What landed on disk parses back identically through the read side.
        let on_disk = read_annotation_file(&e.annotation_path(&root).unwrap()).unwrap().unwrap();
        assert_eq!(on_disk, out.file);
    }

    #[test]
    fn cas_mismatch_is_a_conflict_and_writes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);
        annotate(&root, e, &[set("a", 1.into())], "w", None).unwrap();

        let err = annotate(&root, e, &[set("a", 2.into())], "w", Some(0)).unwrap_err();
        assert!(matches!(err, CatalogError::Conflict { expected: 0, found: 1 }), "{err}");
        let f = read_annotation_file(&e.annotation_path(&root).unwrap()).unwrap().unwrap();
        assert_eq!(f.keys["a"].value, 1, "conflicted write must not land");
        assert_eq!(f.rev, 1);

        // Correct expectation goes through.
        assert_eq!(annotate(&root, e, &[set("a", 2.into())], "w", Some(1)).unwrap().rev, 2);
    }

    #[test]
    fn damaged_file_is_refused_not_clobbered() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);
        let path = e.annotation_path(&root).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ damaged beyond parse").unwrap();

        let err = annotate(&root, e, &[set("a", 1.into())], "w", None).unwrap_err();
        assert!(matches!(err, CatalogError::Invalid(_)), "{err}");
        assert!(err.to_string().contains("damaged"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ damaged beyond parse", "untouched");
    }

    #[test]
    fn version_gate_refuses_unknown_major_write_and_preserves_minor_extras() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);
        let path = e.annotation_path(&root).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, br#"{"format_version":"2.0","keys":{}}"#).unwrap();
        let err = annotate(&root, e, &[set("a", 1.into())], "w", None).unwrap_err();
        assert!(matches!(err, CatalogError::Version { .. }), "{err}");

        // Newer minor: writable, and everything a newer tool wrote survives —
        // the file's own version string, top-level unknowns, and unknown
        // entry fields of keys we did NOT touch. The key we DID overwrite is
        // replaced wholesale (its old triplet described the superseded write).
        std::fs::write(
            &path,
            br#"{
                "format_version": "1.9",
                "rev": 7,
                "keys": {
                    "kept":      {"value": 1, "ts": "t", "writer": "new-tool", "confidence": 0.9},
                    "rewritten": {"value": 2, "ts": "t", "writer": "new-tool", "confidence": 0.4}
                },
                "future_top": {"x": 1}
            }"#,
        )
        .unwrap();
        let out =
            annotate(&root, e, &[set("rewritten", "mine".into())], "old-tool", None).unwrap();
        assert_eq!(out.rev, 8);
        let back: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["format_version"], "1.9", "never downgrade the declared version");
        assert_eq!(back["future_top"]["x"], 1);
        assert_eq!(back["keys"]["kept"]["confidence"], 0.9);
        assert_eq!(back["keys"]["rewritten"]["value"], "mine");
        assert!(
            back["keys"]["rewritten"].get("confidence").is_none(),
            "overwritten entry is replaced wholesale"
        );
    }

    #[test]
    fn protected_keys_refused_per_entity() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        for &k in crate::resolve::PROTECTED_KEYS {
            let err = annotate(&root, AnnotEntity::Record(U), &[set(k, 1.into())], "w", None)
                .unwrap_err();
            assert!(err.to_string().contains("protected"), "{k}: {err}");
        }
        for k in ["id", "started_utc"] {
            let err = annotate(&root, AnnotEntity::Session(U), &[set(k, 1.into())], "w", None)
                .unwrap_err();
            assert!(err.to_string().contains("protected"), "{k}: {err}");
        }
        // The designed repairs stay writable: session_id (attribution),
        // deleted (tombstone) on records; started-time keys differ per entity.
        annotate(&root, AnnotEntity::Record(U), &[set("session_id", Value::Null)], "w", None)
            .unwrap();
        annotate(&root, AnnotEntity::Record(U), &[set("deleted", true.into())], "w", None)
            .unwrap();
        annotate(&root, AnnotEntity::Session(U), &[set("t0_ns", 1.into())], "w", None).unwrap();
        // Unset of a protected key is allowed: it only removes hand-edit
        // garbage, never forges identity.
        annotate(&root, AnnotEntity::Record(U), &[unset("id")], "w", None).unwrap();
    }

    #[test]
    fn empty_batch_empty_key_and_bad_ulid_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        assert!(annotate(&root, AnnotEntity::Record(U), &[], "w", None).is_err());
        assert!(
            annotate(&root, AnnotEntity::Record(U), &[set("  ", 1.into())], "w", None).is_err()
        );
        let err = annotate(&root, AnnotEntity::Record("../etc"), &[set("a", 1.into())], "w", None)
            .unwrap_err();
        assert!(matches!(err, CatalogError::InvalidUlid(_)), "{err}");
    }

    #[test]
    fn unset_reports_missing_and_set_then_unset_respects_order() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);
        let out = annotate(
            &root,
            e,
            &[set("a", 1.into()), unset("a"), unset("ghost")],
            "w",
            None,
        )
        .unwrap();
        assert_eq!(out.unset_missing, vec!["ghost".to_string()]);
        assert!(!out.file.keys.contains_key("a"), "set then unset → gone");
        assert_eq!(out.rev, 1);
    }

    #[test]
    fn journal_records_every_mutation_rev_ordered() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);
        annotate(&root, e, &[set("a", 1.into()), set("b", 2.into())], "w", None).unwrap();
        annotate(&root, e, &[unset("a")], "w", None).unwrap();

        let text = std::fs::read_to_string(root.annotation_journal()).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["verb"], "set");
        assert_eq!(lines[0]["target"], format!("records/{U}"));
        assert_eq!(lines[0]["rev"], 1);
        assert_eq!(lines[1]["rev"], 1, "batch lines share the committed rev");
        assert_eq!(lines[2]["verb"], "unset");
        assert_eq!(lines[2]["rev"], 2);
    }

    #[test]
    fn journal_failure_warns_but_write_commits() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        // Occupy `_journal` with a file so the journal can't be created.
        std::fs::write(root.path().join("_journal"), b"blocker").unwrap();
        let out =
            annotate(&root, AnnotEntity::Record(U), &[set("a", 1.into())], "w", None).unwrap();
        assert_eq!(out.rev, 1, "annotation write must survive journal failure");
        assert!(out.journal_warning.is_some());
    }

    #[test]
    fn rev_at_max_is_refused_not_wrapped() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let e = AnnotEntity::Record(U);
        let path = e.annotation_path(&root).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(r#"{{"format_version":"1.0","rev":{},"keys":{{}}}}"#, u64::MAX),
        )
        .unwrap();
        let err = annotate(&root, e, &[set("a", 1.into())], "w", None).unwrap_err();
        assert!(err.to_string().contains("u64::MAX"), "{err}");
        // File untouched — never wrapped to rev 0.
        let f = read_annotation_file(&path).unwrap().unwrap();
        assert_eq!(f.rev, u64::MAX);
    }

    #[test]
    fn concurrent_writers_on_separate_handles_lose_nothing() {
        // Each annotate() call opens its own fd onto the annotations/ dir and
        // flocks it; two threads here exercise exactly the same exclusion two
        // processes get (the CLI E2E proves the real two-process case end to
        // end).
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        std::thread::scope(|s| {
            for w in 0..2 {
                let root = &root;
                s.spawn(move || {
                    for i in 0..50 {
                        annotate(
                            root,
                            AnnotEntity::Record(U),
                            &[set(&format!("k{w}_{i}"), i.into())],
                            &format!("writer{w}"),
                            None,
                        )
                        .unwrap();
                    }
                });
            }
        });
        let f = read_annotation_file(&AnnotEntity::Record(U).annotation_path(&root).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(f.rev, 100, "every batch bumped exactly once");
        assert_eq!(f.keys.len(), 100, "no read-modify-write interleave lost a key");
    }
}
