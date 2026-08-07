//! Capture-machine garbage collection: reclaim record directories that have
//! been verified-backed-up to the analysis machine AND are past the retention
//! window. Both criteria are required — neither is sufficient alone (the
//! ratified "GC 双判据缺一不可"):
//!
//!   1. the record's ULID is acked in `_sync/synced.json` (the analysis
//!      machine hash-verified a copy — an rsync exit code is NOT proof, which
//!      is why the ack, not the transfer's success, is the criterion);
//!   2. the record is older than the retention period (default 7 days,
//!      anchored on `provenance.created_utc`), so a just-captured record is
//!      never reclaimed even if it somehow raced into the ack.
//!
//! Camera-card model: the capture machine is a short-term buffer, so deleting
//! *good* records is the whole point — they are backed up. Tombstones live on
//! the analysis machine (`annotations/`, never synced here), so gc neither
//! sees nor needs them. Sessions (`session.json`) are never collected: they
//! are tiny and attribution needs them.
//!
//! Destruction requires proof (mirrors [`crate::adopt`]): anything gc cannot
//! positively assess is KEPT — an unreadable/version-refused manifest, a
//! directory with subdirectories (v1 artifacts are flat; a subdir is unscanned
//! and so never "provably nothing of value"), a non-ULID name, or a symlinked
//! entry (never followed into a delete).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::id::{is_valid_ulid, ulid_shard};
use crate::layout::DataRoot;
use crate::manifest::{RecordManifest, RECORD_MANIFEST, SESSION_MANIFEST};
use crate::sync::SyncAck;
use crate::CatalogError;

/// Default retention window: a record younger than this is never collected,
/// even when acked. ~7 days covers "I might still want to look at last week's
/// captures on the card before they're reclaimed".
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One record the plan would collect.
#[derive(Debug, Clone, Serialize)]
pub struct GcCandidate {
    pub ulid: String,
    /// Directory relative to the data root (for display).
    pub rel: String,
    pub created_utc: String,
    /// First-verified timestamp from the ack (when the analysis machine
    /// confirmed a hash-verified copy).
    pub verified_utc: String,
    /// Bytes the record directory occupies (reclaimed on delete).
    pub size_bytes: u64,
    /// Absolute directory path — not serialized (internal to apply).
    #[serde(skip)]
    pub dir: PathBuf,
}

/// The read-only plan: what would be collected and why everything else is kept.
#[derive(Debug, Default, Serialize)]
pub struct GcPlan {
    pub collectable: Vec<GcCandidate>,
    pub total_reclaimable: u64,
    /// Record directories scanned (excludes sessions, non-ULID, symlinks).
    pub scanned: u64,
    /// Kept: not (yet) acked as verified-backed-up.
    pub kept_unverified: u64,
    /// Kept: acked, but still within the retention window.
    pub kept_too_young: u64,
    /// Kept: could not be positively assessed (no/unreadable/version-refused
    /// manifest, subdirectory present, unparseable created_utc).
    pub kept_unassessable: u64,
    /// Session directories skipped (never collected).
    pub sessions_skipped: u64,
}

/// One per-candidate failure during apply (the batch never aborts on one).
#[derive(Debug, Clone, Serialize)]
pub struct GcFailure {
    pub ulid: String,
    pub error: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GcOutcome {
    pub deleted: Vec<String>,
    pub freed_bytes: u64,
    pub failed: Vec<GcFailure>,
}

/// Plan a GC: scan `records/`, apply both criteria against `ack` at `now`.
/// Read-only — touches nothing. `ack = None` (no `synced.json`) means nothing
/// is proven backed-up, so nothing is collectable (every record counts as
/// `kept_unverified`).
pub fn plan_gc(
    root: &DataRoot,
    ack: Option<&SyncAck>,
    retention: Duration,
    now: SystemTime,
) -> Result<GcPlan, CatalogError> {
    let mut plan = GcPlan::default();
    let shards = match fs::read_dir(root.records_dir()) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(plan),
        Err(e) => return Err(e.into()),
    };
    for shard in shards {
        let shard = shard?;
        // file_type() does NOT follow symlinks; a stray file or a symlinked
        // "shard" is is_dir()==false and skipped (reindex is what reports it).
        if !shard.file_type()?.is_dir() {
            continue;
        }
        let shard_name = shard.file_name().to_string_lossy().into_owned();
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            // A symlinked record directory is is_dir()==false here, so gc
            // never recurses into or removes through it.
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Must be a ULID sitting in its own shard — same identity gate as
            // reindex. Anything else is left entirely alone.
            if !is_valid_ulid(&name) || ulid_shard(&name) != shard_name {
                continue;
            }
            plan.scanned += 1;
            assess_one(&entry.path(), &name, &shard_name, ack, retention, now, &mut plan)?;
        }
    }
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
fn assess_one(
    dir: &Path,
    ulid: &str,
    shard_name: &str,
    ack: Option<&SyncAck>,
    retention: Duration,
    now: SystemTime,
    plan: &mut GcPlan,
) -> Result<(), CatalogError> {
    // One pass over the directory: identify it (record vs session), refuse to
    // collect anything with a subdirectory (unscanned → unprovable), and sum
    // the reclaimable size.
    let mut has_record = false;
    let mut has_session = false;
    let mut subdirs = 0usize;
    let mut size = 0u64;
    for e in fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        if ft.is_dir() {
            subdirs += 1;
            continue;
        }
        let n = e.file_name().to_string_lossy().into_owned();
        if n == RECORD_MANIFEST {
            has_record = true;
        } else if n == SESSION_MANIFEST {
            has_session = true;
        }
        size += e.metadata().map(|m| m.len()).unwrap_or(0);
    }

    // A session directory (and not also a record): never collected.
    if has_session && !has_record {
        plan.sessions_skipped += 1;
        return Ok(());
    }
    // No final record manifest (recording-in-progress, conflicting identity,
    // orphan files) or a subdirectory present → cannot prove it is a complete,
    // self-contained record. Keep.
    if !has_record || subdirs > 0 {
        plan.kept_unassessable += 1;
        return Ok(());
    }

    // Criterion 1: the analysis machine acked a verified copy.
    let Some(verified_utc) = ack.and_then(|a| a.verified.get(ulid)) else {
        plan.kept_unverified += 1;
        return Ok(());
    };

    // Criterion 2: past the retention window, anchored on capture time.
    let Some(created_utc) = read_created_utc(&dir.join(RECORD_MANIFEST)) else {
        plan.kept_unassessable += 1; // unreadable/version-refused manifest
        return Ok(());
    };
    let Ok(created) = humantime::parse_rfc3339(&created_utc) else {
        plan.kept_unassessable += 1;
        return Ok(());
    };
    // A created_utc in the future (clock skew) yields age 0 → too young → kept.
    let age = now.duration_since(created).unwrap_or(Duration::ZERO);
    if age < retention {
        plan.kept_too_young += 1;
        return Ok(());
    }

    plan.total_reclaimable += size;
    plan.collectable.push(GcCandidate {
        ulid: ulid.to_string(),
        rel: format!("records/{shard_name}/{ulid}"),
        created_utc,
        verified_utc: verified_utc.clone(),
        size_bytes: size,
        dir: dir.to_path_buf(),
    });
    Ok(())
}

/// Read `provenance.created_utc` from a record manifest, returning `None` for
/// any reason it can't be trusted (missing, unparseable, version-refused) —
/// all of which keep the record (cannot prove its age).
fn read_created_utc(manifest: &Path) -> Option<String> {
    let bytes = fs::read(manifest).ok()?;
    let m = RecordManifest::from_slice(&bytes).ok()?;
    Some(m.provenance.created_utc)
}

/// Execute a plan: remove each candidate's record directory. Never aborts the
/// batch on one failure. Re-checks at delete time that the target is a real
/// directory and NOT a symlink (`symlink_metadata` does not follow), so a link
/// planted between plan and apply can never redirect the delete outside the
/// records tree.
pub fn apply_gc(plan: &GcPlan) -> GcOutcome {
    let mut out = GcOutcome::default();
    for c in &plan.collectable {
        match fs::symlink_metadata(&c.dir) {
            Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {}
            Ok(_) => {
                out.failed.push(GcFailure {
                    ulid: c.ulid.clone(),
                    error: "no longer a plain directory (symlink/file) — skipped".into(),
                });
                continue;
            }
            Err(e) => {
                out.failed.push(GcFailure { ulid: c.ulid.clone(), error: format!("stat: {e}") });
                continue;
            }
        }
        match fs::remove_dir_all(&c.dir) {
            Ok(()) => {
                out.deleted.push(c.ulid.clone());
                out.freed_bytes += c.size_bytes;
            }
            Err(e) => out.failed.push(GcFailure { ulid: c.ulid.clone(), error: e.to_string() }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::UlidGen;
    use crate::manifest::tests_support::{complete_manifest, npy_bytes};
    use crate::writer::{commit_complete_record, ArtifactPayload};

    fn ts(s: &str) -> SystemTime {
        humantime::parse_rfc3339(s).unwrap()
    }

    /// Commit one record with a chosen capture time; returns its ULID.
    fn fab(root: &DataRoot, gen: &mut UlidGen, created_utc: &str) -> String {
        let npy = npy_bytes(8, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.id = gen.next_now();
        m.provenance.created_utc = created_utc.into();
        m.artifacts.clear();
        commit_complete_record(
            root,
            &mut m,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &npy,
            }],
        )
        .unwrap();
        m.id
    }

    fn ack_of(host: &str, pairs: &[(&str, &str)]) -> SyncAck {
        let mut a = SyncAck::new();
        a.host = Some(host.into());
        for (u, t) in pairs {
            a.verified.insert((*u).into(), (*t).into());
        }
        a
    }

    #[test]
    fn empty_root_plans_nothing() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let plan = plan_gc(&root, None, DEFAULT_RETENTION, ts("2026-06-13T00:00:00Z")).unwrap();
        assert_eq!(plan.scanned, 0);
        assert!(plan.collectable.is_empty());
    }

    #[test]
    fn both_criteria_required_neither_sufficient() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        // old + verified → collect; old + unverified → keep; young + verified → keep.
        let old_v = fab(&root, &mut gen, "2026-06-01T00:00:00.000Z");
        let old_unv = fab(&root, &mut gen, "2026-06-01T00:00:00.000Z");
        let young_v = fab(&root, &mut gen, "2026-06-12T00:00:00.000Z");
        let now = ts("2026-06-13T00:00:00Z");
        let ack = ack_of("mac", &[(&old_v, "2026-06-02T00:00:00Z"), (&young_v, "2026-06-12T12:00:00Z")]);

        let plan = plan_gc(&root, Some(&ack), DEFAULT_RETENTION, now).unwrap();
        let ids: Vec<&str> = plan.collectable.iter().map(|c| c.ulid.as_str()).collect();
        assert_eq!(ids, vec![old_v.as_str()], "only old AND verified");
        assert_eq!(plan.kept_unverified, 1, "{:?}", plan); // old_unv
        assert_eq!(plan.kept_too_young, 1); // young_v
        assert!(plan.collectable[0].size_bytes > 0);
        assert_eq!(plan.total_reclaimable, plan.collectable[0].size_bytes);
        let _ = old_unv;
    }

    #[test]
    fn no_ack_collects_nothing() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        fab(&root, &mut gen, "2026-01-01T00:00:00.000Z"); // ancient
        let plan = plan_gc(&root, None, DEFAULT_RETENTION, ts("2026-06-13T00:00:00Z")).unwrap();
        assert!(plan.collectable.is_empty(), "no synced.json → nothing collectable");
        assert_eq!(plan.kept_unverified, 1);
    }

    #[test]
    fn sessions_subdirs_and_unreadable_are_kept() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let now = ts("2026-06-13T00:00:00Z");

        // A session manifest, acked + old, must never be collected.
        let sid = gen.next_now();
        let s = crate::manifest::SessionManifest {
            format_version: crate::manifest::FORMAT_VERSION.into(),
            id: sid.clone(),
            started_utc: "2026-01-01T00:00:00.000Z".into(),
            graph: None,
            host: "arch".into(),
            attrs: Default::default(),
            sink_attrs: Default::default(),
            extra: Default::default(),
        };
        crate::atomic::write_atomic(&root.session_manifest(&sid).unwrap(), &s.to_json_vec().unwrap())
            .unwrap();

        // A record dir, acked + old, but with a subdirectory → unassessable.
        let withsub = fab(&root, &mut gen, "2026-01-01T00:00:00.000Z");
        fs::create_dir(root.record_dir(&withsub).unwrap().join("extra")).unwrap();

        let ack = ack_of("mac", &[(&sid, "t"), (&withsub, "t")]);
        let plan = plan_gc(&root, Some(&ack), DEFAULT_RETENTION, now).unwrap();
        assert!(plan.collectable.is_empty(), "session + subdir-record kept: {plan:?}");
        assert_eq!(plan.sessions_skipped, 1);
        assert_eq!(plan.kept_unassessable, 1);
    }

    #[test]
    fn future_created_is_too_young_not_collected() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let u = fab(&root, &mut gen, "2027-01-01T00:00:00.000Z"); // future
        let ack = ack_of("mac", &[(&u, "t")]);
        let plan = plan_gc(&root, Some(&ack), DEFAULT_RETENTION, ts("2026-06-13T00:00:00Z")).unwrap();
        assert!(plan.collectable.is_empty());
        assert_eq!(plan.kept_too_young, 1, "clock-skew future ts must not collect");
    }

    #[test]
    fn apply_removes_only_the_planned_dirs() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let collect = fab(&root, &mut gen, "2026-06-01T00:00:00.000Z");
        let keep = fab(&root, &mut gen, "2026-06-12T00:00:00.000Z"); // too young
        let ack = ack_of("mac", &[(&collect, "t"), (&keep, "t")]);
        let plan = plan_gc(&root, Some(&ack), DEFAULT_RETENTION, ts("2026-06-13T00:00:00Z")).unwrap();

        let out = apply_gc(&plan);
        assert_eq!(out.deleted, vec![collect.clone()]);
        assert!(out.freed_bytes > 0);
        assert!(out.failed.is_empty());
        assert!(!root.record_dir(&collect).unwrap().exists(), "collected dir gone");
        assert!(root.record_dir(&keep).unwrap().exists(), "young record untouched");
    }
}
