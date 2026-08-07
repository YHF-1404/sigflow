//! Reindex adoption: classify the residue a crash (or normal operation) can
//! leave in a record directory, as a pure decision function over scanned
//! facts plus the IO scanner that gathers them.
//!
//! The ratified adoption table:
//!
//! | manifest          | artifacts                | verdict                       |
//! |-------------------|--------------------------|-------------------------------|
//! | final             | all verified (≥1)        | Complete                      |
//! | final             | missing/corrupt/none     | Corrupt — alert, never delete |
//! | final (unparsable)| any                      | Corrupt — alert, never delete |
//! | recording         | all valid (≥1)           | Salvage{partial:false}        |
//! | recording         | some valid               | Salvage{partial:true}         |
//! | recording         | none valid / none        | FailedEmpty — delete dir      |
//! | none              | files present            | Orphan — alert, never adopt   |
//! | none              | nothing (or tmp only)    | EmptyDir — delete dir         |
//! | session.json      | (no artifacts expected)  | Complete / Corrupt            |
//! | record.json AND session.json | any           | Corrupt — conflicting identity, keep everything |
//! | any manifest with a newer-major version | any | Corrupt — upgrade the tool, never delete |
//! | (any) + a subdirectory | any                 | deletion verdicts forbidden — see below |
//!
//! Subdirectories are never scanned (v1 artifacts are flat), and what is
//! never read can never be "provably nothing of value": any subdirectory
//! downgrades the would-be-deleting verdicts (EmptyDir/FailedEmpty) to
//! Orphan, so a future v2 layout, sync residue or hand-tidied files are
//! alerted about and kept, not recursively destroyed.
//!
//! `.tmp` residue is always deletable (atomic writes never expose a partial
//! file under its final name). A stale `record.json.recording` next to a
//! *parsable* final manifest is deletable — only a parsable final is
//! authoritative; next to a damaged final, the `.recording` may be the one
//! manifest a human can still salvage from, so it is kept.
//!
//! Destruction requires proof: only `FailedEmpty`/`EmptyDir` (provably
//! nothing of value) ever delete; everything doubtful alerts and keeps. A
//! read error is not proof — the scanner aborts (`Err`) instead of letting
//! an unreadable file count as worthless.
//!
//! Liveness caveat: against a live data root the sink may be mid-write
//! (artifacts present, manifest not yet renamed). Callers acting on
//! `EmptyDir`/`FailedEmpty`/`Orphan` must exclude recently-modified
//! directories (the S4 reindex applies an age guard before acting).

use std::fs;
use std::io;
use std::path::Path;

use crate::manifest::{
    RecordManifest, SessionManifest, RECORDING_SUFFIX, RECORD_MANIFEST, SESSION_MANIFEST,
    TMP_SUFFIX,
};
use crate::CatalogError;

/// Per-file health, gathered by the scanner.
///
/// For a *final* manifest the entries describe the declared artifacts
/// (hash-verified). For a *recording* manifest they describe the data files
/// actually present (self-validated — hashes are optional until complete).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStatus {
    Verified,
    Corrupt,
    Missing,
}

/// What the scanner found in one record directory.
#[derive(Debug, Clone, Default)]
pub struct DirFacts {
    pub has_final: bool,
    pub final_parses: bool,
    pub has_recording: bool,
    pub recording_parses: bool,
    /// The final manifest is a `session.json` (sessions declare no artifacts).
    pub is_session: bool,
    /// Both `record.json` and `session.json` are present — conflicting
    /// identity; neither is trusted and nothing may be cleaned up.
    pub conflicting_finals: bool,
    /// The manifest was refused because its `format_version` major is newer
    /// than this tool reads (holds the refused version). The verdict is the
    /// same as Corrupt (alert, keep), but the right operator action is
    /// "upgrade the tool", not "investigate bit rot" — alerts must say so.
    pub version_refused: Option<String>,
    /// See [`ArtifactStatus`] for whose artifacts these are.
    pub artifacts: Vec<ArtifactStatus>,
    /// Data files present but not declared by a parsed final manifest, or
    /// any data files when no manifest parses at all.
    pub undeclared_files: usize,
    pub tmp_files: usize,
    /// Subdirectories present. Never scanned — and therefore never proven
    /// worthless: any subdirectory forbids the deleting verdicts.
    pub subdirs: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Healthy: index it.
    Complete,
    /// A final manifest whose promise is broken (unreadable manifest,
    /// missing/corrupt artifacts, conflicting identity, or a newer-major
    /// version — see `DirFacts::version_refused` for which alert to raise).
    /// Alert; never auto-delete.
    Corrupt,
    /// Recording residue with usable data: rewrite a final manifest with
    /// `state=salvaged` (partial: some files were invalid and dropped).
    Salvage { partial: bool },
    /// Recording residue with nothing of value: delete the directory.
    FailedEmpty,
    /// Data files without any usable manifest. Alert; never auto-adopt
    /// (legacy flat captures go through the dedicated migration, not here).
    Orphan,
    /// Nothing but (at most) tmp residue: delete the directory.
    EmptyDir,
}

/// Verdict plus the always-safe cleanup actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub verdict: Verdict,
    /// `.tmp` files may always be removed (never a final name) — except
    /// under a conflicting identity, where nothing is touched.
    pub delete_tmp: bool,
    /// A `record.json.recording` shadowed by a *parsable* final manifest is
    /// stale (the final fully supersedes it). Never set when the final is
    /// unparsable — the recording may then be the only salvageable manifest.
    pub delete_stale_recording: bool,
}

/// The adoption table as a pure function. See module docs for the table.
pub fn adjudicate(f: &DirFacts) -> Adoption {
    // Conflicting identity: a directory claiming to be both a record and a
    // session is exactly the can't-happen state reindex exists to surface.
    // Deterministic Corrupt, and no cleanup of any kind — even tmp files are
    // potential evidence here.
    if f.conflicting_finals {
        return Adoption {
            verdict: Verdict::Corrupt,
            delete_tmp: false,
            delete_stale_recording: false,
        };
    }

    let delete_tmp = f.tmp_files > 0;

    // A refused format_version is NOT damage — the manifest was written by a
    // newer tool and this one cannot judge it. Alert ("upgrade the tool")
    // and touch nothing: a v2 `.recording` must not fall through to
    // FailedEmpty/Orphan, and no sibling may be cleaned up on a manifest we
    // could not read.
    if f.version_refused.is_some() {
        return Adoption {
            verdict: Verdict::Corrupt,
            delete_tmp,
            delete_stale_recording: false,
        };
    }

    let delete_stale_recording = f.has_final && f.final_parses && f.has_recording;
    // What was never read can never be "provably nothing of value": any
    // subdirectory forbids the deleting verdicts (see module docs).
    let has_unscanned = f.subdirs > 0;

    let verdict = if f.has_final {
        if !f.final_parses {
            Verdict::Corrupt
        } else if f.is_session {
            // Sessions declare no artifacts; a parsed manifest is complete.
            Verdict::Complete
        } else if !f.artifacts.is_empty()
            && f.artifacts.iter().all(|s| *s == ArtifactStatus::Verified)
        {
            Verdict::Complete
        } else {
            // Zero artifacts declared, or any missing/corrupt.
            Verdict::Corrupt
        }
    } else if f.has_recording {
        if !f.recording_parses {
            // A half-written recording manifest cannot exist (tmp+rename);
            // an unparsable one means external damage. Keep anything present.
            if f.undeclared_files > 0 || has_unscanned {
                Verdict::Orphan
            } else {
                Verdict::FailedEmpty
            }
        } else {
            let valid = f.artifacts.iter().filter(|s| **s == ArtifactStatus::Verified).count();
            let invalid = f.artifacts.len() - valid;
            if valid > 0 {
                Verdict::Salvage { partial: invalid > 0 }
            } else if has_unscanned {
                Verdict::Orphan
            } else {
                Verdict::FailedEmpty
            }
        }
    } else if f.undeclared_files > 0 || has_unscanned {
        Verdict::Orphan
    } else {
        Verdict::EmptyDir
    };

    Adoption { verdict, delete_tmp, delete_stale_recording }
}

/// Scan outcome: the facts, the ruling, and the parsed final record manifest
/// when there is one (for the indexer).
#[derive(Debug)]
pub struct ScanOutcome {
    pub facts: DirFacts,
    pub adoption: Adoption,
    pub manifest: Option<RecordManifest>,
    /// The parsed `record.json.recording`, when present and parsable —
    /// salvage rewrites a final manifest from it.
    pub recording: Option<RecordManifest>,
    /// Data files seen in the directory (everything that is not a manifest
    /// or `.tmp`). In the recording-residue case this is index-aligned with
    /// `facts.artifacts`, so salvage knows which file each status judged.
    pub data_files: Vec<String>,
}

/// Scan one record directory and adjudicate it. Read-only: callers decide
/// whether to apply the verdict's actions (and must apply the liveness age
/// guard first — see module docs).
///
/// Returns `Err` on any IO failure other than not-found — an unreadable
/// file is *not* evidence of worthlessness, so the scan refuses to rule
/// rather than risk a destructive verdict (alert and keep).
pub fn check_record_dir(dir: &Path) -> io::Result<ScanOutcome> {
    let mut facts = DirFacts::default();
    let mut data_files: Vec<String> = Vec::new();
    let mut has_record_final = false;
    let mut has_session_final = false;

    let recording_name = format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}");
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            // v1 artifacts are flat; subdirs are not scanned but COUNT —
            // their existence forbids the deleting verdicts.
            facts.subdirs += 1;
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(TMP_SUFFIX) {
            facts.tmp_files += 1;
        } else if name == RECORD_MANIFEST {
            has_record_final = true;
        } else if name == SESSION_MANIFEST {
            has_session_final = true;
        } else if name == recording_name {
            facts.has_recording = true;
        } else {
            data_files.push(name);
        }
    }

    facts.has_final = has_record_final || has_session_final;
    facts.is_session = has_session_final && !has_record_final;
    facts.conflicting_finals = has_record_final && has_session_final;

    let mut manifest = None;
    let mut recording = None;
    if facts.conflicting_finals {
        // Neither identity is trusted; the verdict does not depend on their
        // contents, so nothing is read.
        facts.undeclared_files = data_files.len();
    } else if facts.has_final {
        let path = if facts.is_session {
            dir.join(SESSION_MANIFEST)
        } else {
            dir.join(RECORD_MANIFEST)
        };
        let bytes = fs::read(&path)?;
        if facts.is_session {
            match SessionManifest::from_slice(&bytes) {
                Ok(_) => facts.final_parses = true,
                Err(CatalogError::Version { found }) => facts.version_refused = Some(found),
                Err(_) => {}
            }
        } else {
            match RecordManifest::from_slice(&bytes) {
                Ok(m) => {
                    facts.final_parses = true;
                    // Verify every declared artifact; count undeclared leftovers.
                    for a in &m.artifacts {
                        facts.artifacts.push(verify_declared(
                            dir,
                            &a.path,
                            a.size_bytes,
                            a.blake3.as_deref(),
                        )?);
                    }
                    facts.undeclared_files = data_files
                        .iter()
                        .filter(|f| !m.artifacts.iter().any(|a| &a.path == *f))
                        .count();
                    manifest = Some(m);
                }
                Err(CatalogError::Version { found }) => facts.version_refused = Some(found),
                Err(_) => {}
            }
        }
        if !facts.final_parses {
            facts.undeclared_files = data_files.len();
        }
    } else if facts.has_recording {
        let bytes = fs::read(dir.join(&recording_name))?;
        match RecordManifest::from_slice(&bytes) {
            Ok(m) => {
                facts.recording_parses = true;
                recording = Some(m);
            }
            Err(CatalogError::Version { found }) => facts.version_refused = Some(found),
            Err(_) => {}
        }
        if facts.recording_parses {
            // Hashes are optional-until-complete: self-validate what exists.
            for f in &data_files {
                facts.artifacts.push(self_validate(&dir.join(f))?);
            }
        } else {
            facts.undeclared_files = data_files.len();
        }
    } else {
        facts.undeclared_files = data_files.len();
    }

    let adoption = adjudicate(&facts);
    Ok(ScanOutcome { facts, adoption, manifest, recording, data_files })
}

/// Verify a declared artifact: present → size → blake3. Only a confirmed
/// not-found maps to `Missing`; any other read error aborts the scan.
fn verify_declared(
    dir: &Path,
    rel: &str,
    size: u64,
    hash: Option<&str>,
) -> io::Result<ArtifactStatus> {
    let path = dir.join(rel);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ArtifactStatus::Missing),
        Err(e) => return Err(e),
    };
    if bytes.len() as u64 != size {
        return Ok(ArtifactStatus::Corrupt);
    }
    Ok(match hash {
        Some(h) if blake3::hash(&bytes).to_hex().as_str() == h => ArtifactStatus::Verified,
        Some(_) => ArtifactStatus::Corrupt,
        // A final manifest must carry hashes (the committing APIs —
        // promote_validated / commit_complete_record — enforce it); one
        // without is a broken promise.
        None => ArtifactStatus::Corrupt,
    })
}

/// Self-validation for recording residue, where no hash exists yet. Only a
/// provably broken file is condemned: `.npy` must be header-consistent,
/// `.json` must parse; unknown formats are kept. A read error aborts the
/// scan — never counts as invalid (it could feed a FailedEmpty deletion).
fn self_validate(path: &Path) -> io::Result<ArtifactStatus> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ArtifactStatus::Missing),
        Err(e) => return Err(e),
    };
    Ok(match path.extension().and_then(|e| e.to_str()) {
        Some("npy") => {
            if npy_consistent(&bytes) {
                ArtifactStatus::Verified
            } else {
                ArtifactStatus::Corrupt
            }
        }
        Some("json") => {
            if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() {
                ArtifactStatus::Verified
            } else {
                ArtifactStatus::Corrupt
            }
        }
        _ => ArtifactStatus::Verified,
    })
}

/// Check a NumPy file's header against its actual length. Condemns ONLY
/// provable damage — anything this code cannot positively interpret is kept:
///
/// - no NUMPY magic (covers the ext4 size-correct-but-zero-page crash case)
///   → broken;
/// - truncated header, or total length ≠ header + shape×itemsize when both
///   descr and shape are understood → broken;
/// - unknown format major version, exotic descr (structured dtypes,
///   unicode), unparsable shape → kept (not proof of damage).
fn npy_consistent(bytes: &[u8]) -> bool {
    if bytes.len() < 10 || &bytes[0..6] != b"\x93NUMPY" {
        return false;
    }
    let (hlen, header_start) = match bytes[6] {
        1 => (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10),
        2 | 3 => {
            if bytes.len() < 12 {
                return false;
            }
            (
                u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize,
                12,
            )
        }
        _ => return true, // future format version: cannot prove damage
    };
    let Some(header) = bytes.get(header_start..header_start + hlen) else {
        return false; // header longer than the file: provably truncated
    };
    let header = String::from_utf8_lossy(header);
    let Some(itemsize) = parse_itemsize(&header) else {
        return true; // exotic dtype: cannot prove damage
    };
    let Some(count) = parse_shape_product(&header) else {
        return true; // unparsable shape: cannot prove damage
    };
    bytes.len() == header_start + hlen + count * itemsize
}

/// Itemsize of a simple `descr` like `<f4` / `<i8` / `|u1` — the trailing
/// digits are the byte width. Unicode (`<U10` = 10 chars × 4 bytes) and
/// structured dtypes return None (handled as "cannot prove damage").
fn parse_itemsize(header: &str) -> Option<usize> {
    let s = header.split("'descr':").nth(1)?;
    let rest = &s[s.find('\'')? + 1..];
    let descr = &rest[..rest.find('\'')?];
    let kind = descr.trim_start_matches(['<', '>', '|', '=']).chars().next()?;
    if !matches!(kind, 'f' | 'i' | 'u' | 'b' | 'c') {
        return None; // U/S/V and anything else: digits ≠ bytes or unknown
    }
    let digits: String = descr.chars().rev().take_while(char::is_ascii_digit).collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

/// Product of all dimensions in `'shape': (a, b, …)` — any rank, including
/// 1-D `(n,)` and 0-D `()` (one element).
fn parse_shape_product(header: &str) -> Option<usize> {
    let s = header.split("'shape':").nth(1)?;
    let inside = s.split('(').nth(1)?.split(')').next()?;
    let mut product: usize = 1;
    for part in inside.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        product = product.checked_mul(part.parse().ok()?)?;
    }
    Some(product)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::{complete_manifest, npy_bytes};
    use crate::manifest::{STATE_COMPLETE, STATE_RECORDING};

    // ---- pure adjudication: one test per table row ----

    fn facts(
        has_final: bool,
        final_parses: bool,
        has_recording: bool,
        recording_parses: bool,
        artifacts: &[ArtifactStatus],
        undeclared: usize,
        tmp: usize,
    ) -> DirFacts {
        DirFacts {
            has_final,
            final_parses,
            has_recording,
            recording_parses,
            artifacts: artifacts.to_vec(),
            undeclared_files: undeclared,
            tmp_files: tmp,
            ..Default::default()
        }
    }

    use ArtifactStatus::*;

    #[test]
    fn table_final_rows() {
        // final + all verified → Complete
        let a = adjudicate(&facts(true, true, false, false, &[Verified], 0, 0));
        assert_eq!(a.verdict, Verdict::Complete);
        // final + a corrupt artifact → Corrupt
        let a = adjudicate(&facts(true, true, false, false, &[Verified, Corrupt], 0, 0));
        assert_eq!(a.verdict, Verdict::Corrupt);
        // final + a missing artifact → Corrupt
        let a = adjudicate(&facts(true, true, false, false, &[Missing], 0, 0));
        assert_eq!(a.verdict, Verdict::Corrupt);
        // final declaring zero artifacts → Corrupt
        let a = adjudicate(&facts(true, true, false, false, &[], 0, 0));
        assert_eq!(a.verdict, Verdict::Corrupt);
        // final that does not parse → Corrupt
        let a = adjudicate(&facts(true, false, false, false, &[], 3, 0));
        assert_eq!(a.verdict, Verdict::Corrupt);
    }

    #[test]
    fn table_recording_rows() {
        // recording + all valid → Salvage full
        let a = adjudicate(&facts(false, false, true, true, &[Verified, Verified], 0, 0));
        assert_eq!(a.verdict, Verdict::Salvage { partial: false });
        // recording + some valid → Salvage partial
        let a = adjudicate(&facts(false, false, true, true, &[Verified, Corrupt], 0, 0));
        assert_eq!(a.verdict, Verdict::Salvage { partial: true });
        // recording + none valid → FailedEmpty
        let a = adjudicate(&facts(false, false, true, true, &[Corrupt], 0, 0));
        assert_eq!(a.verdict, Verdict::FailedEmpty);
        // recording + no files → FailedEmpty
        let a = adjudicate(&facts(false, false, true, true, &[], 0, 0));
        assert_eq!(a.verdict, Verdict::FailedEmpty);
        // unparsable recording + files → keep as Orphan
        let a = adjudicate(&facts(false, false, true, false, &[], 2, 0));
        assert_eq!(a.verdict, Verdict::Orphan);
        // unparsable recording + nothing → FailedEmpty
        let a = adjudicate(&facts(false, false, true, false, &[], 0, 0));
        assert_eq!(a.verdict, Verdict::FailedEmpty);
    }

    #[test]
    fn table_no_manifest_rows() {
        let a = adjudicate(&facts(false, false, false, false, &[], 2, 0));
        assert_eq!(a.verdict, Verdict::Orphan);
        let a = adjudicate(&facts(false, false, false, false, &[], 0, 1));
        assert_eq!(a.verdict, Verdict::EmptyDir);
        assert!(a.delete_tmp);
        let a = adjudicate(&facts(false, false, false, false, &[], 0, 0));
        assert_eq!(a.verdict, Verdict::EmptyDir);
    }

    #[test]
    fn table_defensive_rows() {
        // PARSABLE final + stale recording → final authoritative, recording deletable
        let a = adjudicate(&facts(true, true, true, true, &[Verified], 0, 0));
        assert_eq!(a.verdict, Verdict::Complete);
        assert!(a.delete_stale_recording);
        // UNPARSABLE final + recording → Corrupt, and the recording must be
        // KEPT: it may be the only manifest a human can salvage from.
        let a = adjudicate(&facts(true, false, true, true, &[], 1, 0));
        assert_eq!(a.verdict, Verdict::Corrupt);
        assert!(!a.delete_stale_recording);
        // session manifest → Complete without artifacts
        let mut f = facts(true, true, false, false, &[], 0, 0);
        f.is_session = true;
        assert_eq!(adjudicate(&f).verdict, Verdict::Complete);
        // conflicting identities → deterministic Corrupt, nothing cleaned up
        let mut f = facts(true, false, true, true, &[], 2, 1);
        f.conflicting_finals = true;
        let a = adjudicate(&f);
        assert_eq!(a.verdict, Verdict::Corrupt);
        assert!(!a.delete_tmp);
        assert!(!a.delete_stale_recording);
    }

    // ---- scanner integration over real directories ----

    fn write_record_dir(
        dir: &Path,
        state: &str,
        final_name: bool,
        rows: usize,
        cols: usize,
        corrupt: bool,
    ) {
        let npy = npy_bytes(rows, cols);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.state = state.to_string();
        std::fs::create_dir_all(dir).unwrap();
        let mut data = npy;
        if corrupt {
            let last = data.len() - 1;
            data[last] ^= 0xff;
        }
        std::fs::write(dir.join("waveform.npy"), &data).unwrap();
        let name = if final_name {
            RECORD_MANIFEST.to_string()
        } else {
            format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")
        };
        std::fs::write(dir.join(name), m.to_json_vec().unwrap()).unwrap();
    }

    /// A NumPy file with arbitrary version/descr/itemsize for the
    /// only-provable-damage tests.
    fn npy_custom(version: u8, descr: &str, itemsize: usize, count: usize) -> Vec<u8> {
        let dict =
            format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': ({count},), }}");
        let preamble = if version == 1 { 10 } else { 12 };
        let base = preamble + dict.len() + 1;
        let pad = (64 - (base % 64)) % 64;
        let hlen = dict.len() + 1 + pad;
        let mut out = Vec::new();
        out.extend_from_slice(b"\x93NUMPY");
        out.push(version);
        out.push(0);
        if version == 1 {
            out.extend_from_slice(&(hlen as u16).to_le_bytes());
        } else {
            out.extend_from_slice(&(hlen as u32).to_le_bytes());
        }
        out.extend_from_slice(dict.as_bytes());
        out.extend(std::iter::repeat_n(b' ', pad));
        out.push(b'\n');
        out.extend(std::iter::repeat_n(0u8, count * itemsize));
        out
    }

    #[test]
    fn scan_complete_record_verifies_hash() {
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_COMPLETE, true, 4, 2, false);
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Complete);
        assert!(out.manifest.is_some());
    }

    #[test]
    fn scan_flipped_byte_is_corrupt_not_deleted() {
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_COMPLETE, true, 4, 2, true);
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Corrupt);
    }

    #[test]
    fn scan_recording_with_valid_npy_salvages() {
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_RECORDING, false, 4, 2, false);
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Salvage { partial: false });
    }

    #[test]
    fn scan_recording_with_truncated_npy_is_failed_empty() {
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_RECORDING, false, 4, 2, false);
        // Truncate the artifact: header promises more bytes than exist.
        let p = t.path().join("waveform.npy");
        let bytes = std::fs::read(&p).unwrap();
        std::fs::write(&p, &bytes[..bytes.len() - 4]).unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::FailedEmpty);
    }

    #[test]
    fn scan_zero_page_npy_detected() {
        // ext4 crash case: size right, content zeroed → no magic → corrupt.
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_RECORDING, false, 4, 2, false);
        let p = t.path().join("waveform.npy");
        let len = std::fs::read(&p).unwrap().len();
        std::fs::write(&p, vec![0u8; len]).unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::FailedEmpty);
    }

    #[test]
    fn subdirectory_forbids_deleting_verdicts() {
        // What was never scanned can never be "provably nothing of value".
        // EmptyDir-with-subdir → Orphan (kept).
        let mut f = facts(false, false, false, false, &[], 0, 0);
        f.subdirs = 1;
        assert_eq!(adjudicate(&f).verdict, Verdict::Orphan);
        // FailedEmpty-with-subdir (parsable recording, nothing valid) → Orphan.
        let mut f = facts(false, false, true, true, &[Corrupt], 0, 0);
        f.subdirs = 1;
        assert_eq!(adjudicate(&f).verdict, Verdict::Orphan);
        // Unparsable recording + subdir → Orphan.
        let mut f = facts(false, false, true, false, &[], 0, 0);
        f.subdirs = 1;
        assert_eq!(adjudicate(&f).verdict, Verdict::Orphan);

        // End to end: a record dir whose only content hides in a subdir
        // must NOT come back as EmptyDir.
        let t = tempfile::tempdir().unwrap();
        let sub = t.path().join("backup");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("waveform.npy"), npy_bytes(4, 2)).unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.facts.subdirs, 1);
        assert_eq!(out.adoption.verdict, Verdict::Orphan);
    }

    #[test]
    fn version_refused_recording_is_corrupt_never_failed_empty() {
        // A `.recording` from a newer tool: this tool cannot judge it, so
        // the verdict is Corrupt (alert "upgrade"), NEVER FailedEmpty —
        // even with no data files at all.
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            br#"{"format_version":"2.0","records":[1]}"#,
        )
        .unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Corrupt);
        assert_eq!(out.facts.version_refused.as_deref(), Some("2.0"));
        assert!(!out.adoption.delete_stale_recording);
    }

    #[test]
    fn scan_orphan_and_empty_and_tmp() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("stray.npy"), b"junk").unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Orphan);

        let t2 = tempfile::tempdir().unwrap();
        std::fs::write(t2.path().join("record.json.tmp"), b"{").unwrap();
        let out = check_record_dir(t2.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::EmptyDir);
        assert!(out.adoption.delete_tmp);
    }

    #[test]
    fn scan_session_dir_completes_without_artifacts() {
        let t = tempfile::tempdir().unwrap();
        let s = crate::manifest::SessionManifest {
            format_version: crate::manifest::FORMAT_VERSION.into(),
            id: "01JXAB3C4D5E6F7G8H9JKMNPQR".into(),
            started_utc: "2026-06-10T08:00:00.000Z".into(),
            graph: None,
            host: "archlinux".into(),
            attrs: Default::default(),
            sink_attrs: Default::default(),
            extra: Default::default(),
        };
        std::fs::write(t.path().join(SESSION_MANIFEST), s.to_json_vec().unwrap()).unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Complete);
        assert!(out.facts.is_session);
    }

    #[test]
    fn scan_dual_manifest_is_deterministic_corrupt_with_no_cleanup() {
        // record.json AND session.json in one dir: conflicting identity.
        // The verdict must not depend on readdir order, and nothing may be
        // cleaned up (not even tmp residue).
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_COMPLETE, true, 4, 2, false);
        std::fs::write(t.path().join(SESSION_MANIFEST), b"{}").unwrap();
        std::fs::write(t.path().join("record.json.tmp"), b"{").unwrap();
        std::fs::write(
            t.path().join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            b"{}",
        )
        .unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert!(out.facts.conflicting_finals);
        assert_eq!(out.adoption.verdict, Verdict::Corrupt);
        assert!(!out.adoption.delete_tmp);
        assert!(!out.adoption.delete_stale_recording);
        assert!(out.manifest.is_none());
    }

    #[test]
    fn scan_corrupt_final_keeps_recording_sibling() {
        // Bit-rotted final + intact recording: Corrupt, and the recording
        // (the only salvageable manifest) must NOT be flagged for deletion.
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_RECORDING, false, 4, 2, false);
        std::fs::write(t.path().join(RECORD_MANIFEST), b"{ truncated garba").unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Corrupt);
        assert!(!out.adoption.delete_stale_recording);
    }

    #[test]
    fn scan_newer_major_is_corrupt_with_version_refused() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join(RECORD_MANIFEST),
            br#"{"format_version":"2.0","records":[1]}"#,
        )
        .unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Corrupt);
        assert_eq!(out.facts.version_refused.as_deref(), Some("2.0"));
    }

    #[test]
    fn npy_only_provable_damage_is_condemned() {
        // Valid <f8 (future dtype change), v2.0 <f4, and 1-D shapes are all
        // KEPT; truncation of an understood layout is condemned.
        assert!(npy_consistent(&npy_custom(1, "<f8", 8, 5)));
        assert!(npy_consistent(&npy_custom(2, "<f4", 4, 5)));
        assert!(npy_consistent(&npy_custom(1, "<f4", 4, 5)));
        // Unknown future format version / exotic dtypes: kept.
        assert!(npy_consistent(&npy_custom(9, "<f4", 4, 5)));
        assert!(npy_consistent(&npy_custom(1, "<U10", 40, 5)));
        // Truncated understood layout: condemned.
        let mut t = npy_custom(1, "<f8", 8, 5);
        t.truncate(t.len() - 1);
        assert!(!npy_consistent(&t));
        let mut t2 = npy_custom(2, "<f4", 4, 5);
        t2.truncate(t2.len() - 2);
        assert!(!npy_consistent(&t2));
        // No magic (zero-page) still condemned.
        assert!(!npy_consistent(&[0u8; 128]));
    }

    #[test]
    fn scan_recording_with_valid_f8_npy_salvages() {
        // End-to-end: a non-<f4 npy must not feed a FailedEmpty deletion.
        let t = tempfile::tempdir().unwrap();
        let npy = npy_custom(1, "<f8", 8, 7);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.state = STATE_RECORDING.to_string();
        std::fs::write(t.path().join("waveform.npy"), &npy).unwrap();
        std::fs::write(
            t.path().join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m.to_json_vec().unwrap(),
        )
        .unwrap();
        let out = check_record_dir(t.path()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Salvage { partial: false });
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_artifact_aborts_scan_instead_of_condemning() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        write_record_dir(t.path(), STATE_RECORDING, false, 4, 2, false);
        let p = t.path().join("waveform.npy");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            return; // running as root: EACCES cannot be simulated
        }
        // EACCES is not proof of worthlessness: the scan must refuse to
        // rule (Err), never return FailedEmpty.
        assert!(check_record_dir(t.path()).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
