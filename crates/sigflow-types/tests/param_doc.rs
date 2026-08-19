//! The configuration-document contract, exercised against a real machine
//! shape: two axes, shared motor/gearbox/encoder libraries, inheritable
//! operating-condition groups, one per-axis override.
//!
//! The fixtures under `fixtures/` are the readable reference for both halves
//! of the format — a schema authored by a plugin, and a document authored by
//! whoever configures the machine.

use serde_json::{json, Value};

use sigflow_types::doc::{self, Provenance};
use sigflow_types::param::ParamValue;
use sigflow_types::schema::{DocSchema, Severity};

fn schema() -> DocSchema {
    toml::from_str(include_str!("fixtures/motor-params.schema.toml"))
        .expect("schema fixture parses")
}

fn document() -> Value {
    toml::from_str(include_str!("fixtures/machine.toml")).expect("document fixture parses")
}

/// `doc.nodes.axis[i]`.
fn axis(doc: &Value, i: usize) -> &Value {
    &doc["nodes"]["axis"][i]
}

fn errors(findings: &[doc::Finding]) -> Vec<&doc::Finding> {
    findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .collect()
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[test]
fn fixtures_parse_into_the_contract_types() {
    let s = schema();
    assert_eq!(s.id, "sigflow.example.servo_axis");
    assert_eq!(s.version, 1);
    // Every library names a group that exists, and every root node resolves.
    for lib in &s.libraries {
        assert!(
            s.group(&lib.group).is_some(),
            "library {} names missing group {}",
            lib.id,
            lib.group
        );
    }
    assert!(s.library("profile").unwrap().inheritable);
    assert!(!s.library("motor").unwrap().inheritable);

    let d = document();
    assert_eq!(d["schema"], json!("sigflow.example.servo_axis"));
    assert_eq!(doc::items(d["nodes"].get("axis")).len(), 2);
}

#[test]
fn sections_and_notes_survive_the_round_trip() {
    let s = schema();
    let profile = s.group("profile").unwrap();
    assert_eq!(profile.sections.len(), 4);
    let kp = profile.field("vel_kp_ua_per_kcps").unwrap();
    assert_eq!(kp.section.as_deref(), Some("vel"));
    assert!(kp.note.as_ref().unwrap().contains("1200"));
    // The bench-proven band is narrower than the legal range.
    assert!(kp.recommended.is_some());

    // Display conversion: stored per-mille, shown as a percentage.
    let torque = profile.field("max_torque_permille").unwrap();
    let display = torque.display.as_ref().unwrap();
    assert_eq!(display.unit.as_deref(), Some("%"));
    assert_eq!(display.scale, Some(0.1));

    // Guards ride on the fields that are protection, not tuning.
    let motor = s.group("motor").unwrap();
    assert!(motor.field("fault_current_ma").unwrap().guard.is_some());
    assert!(motor.field("pole_pairs").unwrap().guard.is_none());

    // Read-back fields are declared, not inferred.
    let rb = s.group("readback").unwrap();
    assert!(rb.fields.iter().all(|f| f.readback));
}

// ---------------------------------------------------------------------------
// Resolution — the override chain and its provenance
// ---------------------------------------------------------------------------

#[test]
fn library_entry_supplies_the_value() {
    let (s, d) = (schema(), document());
    let r = doc::resolve_slot_field(&s, &d, "motor", axis(&d, 0).get("motor"), "pole_pairs");
    assert_eq!(r.value, Some(ParamValue::U32(1)));
    assert_eq!(
        r.provenance,
        Provenance::Entry {
            entry: "dm3510".into()
        }
    );
}

#[test]
fn inheritance_is_followed_but_not_flattened() {
    let (s, d) = (schema(), document());
    let hold = &axis(&d, 0)["profiles"][1];
    assert_eq!(hold["ref"], json!("hold"));

    // Written on `hold` itself.
    let torque = doc::resolve_slot_field(&s, &d, "profile", Some(hold), "max_torque_permille");
    assert_eq!(torque.value, Some(ParamValue::U32(400)));
    assert_eq!(
        torque.provenance,
        Provenance::Entry {
            entry: "hold".into()
        }
    );

    // Reached through `extends = "base"` — and the editor can say so, which
    // is the whole reason the document is not stored flattened.
    let kp = doc::resolve_slot_field(&s, &d, "profile", Some(hold), "vel_kp_ua_per_kcps");
    assert_eq!(kp.value, Some(ParamValue::F64(600.0)));
    assert_eq!(
        kp.provenance,
        Provenance::Inherited {
            entry: "base".into()
        }
    );
}

#[test]
fn instance_override_wins_and_remembers_what_it_replaced() {
    let (s, d) = (schema(), document());
    let r = doc::resolve_slot_field(
        &s,
        &d,
        "motor",
        axis(&d, 1).get("motor"),
        "rated_current_ma",
    );
    assert_eq!(r.value, Some(ParamValue::U32(540)));
    assert_eq!(r.provenance, Provenance::Override);
    // "revert to inherited" needs to show what it would revert *to*.
    assert_eq!(r.inherited, Some(ParamValue::U32(590)));

    // The other axis referencing the same entry is untouched.
    let other = doc::resolve_slot_field(
        &s,
        &d,
        "motor",
        axis(&d, 0).get("motor"),
        "rated_current_ma",
    );
    assert_eq!(other.value, Some(ParamValue::U32(590)));
}

#[test]
fn unset_is_not_zero() {
    let (s, d) = (schema(), document());
    // Firmware reads a written 0 as "use the derived coefficient"; an unset
    // field must therefore stay distinguishable from a zero all the way to
    // the form, or saving the form invents a decision nobody made.
    let r = doc::resolve_slot_field(&s, &d, "motor", axis(&d, 0).get("motor"), "current_kp_q15");
    assert_eq!(r.value, None);
    assert_eq!(r.provenance, Provenance::Unset);
}

#[test]
fn schema_default_fills_in_when_nothing_is_written() {
    let s = schema();
    let cal = s.group("calibration").unwrap();
    let r = doc::resolve_group_field(cal, None, "mode");
    assert_eq!(r.value, Some(ParamValue::Enum("z_find".into())));
    assert_eq!(r.provenance, Provenance::Default);

    // No default and nothing written stays unset rather than becoming 0.0.
    let travel = s.group("travel").unwrap();
    assert_eq!(
        doc::resolve_group_field(travel, None, "min").provenance,
        Provenance::Unset
    );
}

#[test]
fn group_nodes_resolve_locally() {
    let (s, d) = (schema(), document());
    let bus = s.group("bus").unwrap();
    let r = doc::resolve_group_field(bus, doc::node(&d, "bus"), "cycle_us");
    assert_eq!(r.value, Some(ParamValue::U32(1000)));
    assert_eq!(r.provenance, Provenance::Local);
}

#[test]
fn resolve_slot_returns_every_field_in_schema_order() {
    let (s, d) = (schema(), document());
    let fields = doc::resolve_slot(&s, &d, "gearbox", axis(&d, 0).get("gearbox"));
    let ids: Vec<&str> = fields.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ratio_num", "ratio_den", "max_input_rpm", "backlash_arcmin"]
    );
}

// ---------------------------------------------------------------------------
// Library references
// ---------------------------------------------------------------------------

#[test]
fn reference_count_tells_the_operator_what_an_edit_touches() {
    let (s, d) = (schema(), document());
    // Both axes run a dm3510; editing the type changes both.
    assert_eq!(doc::reference_count(&s, &d, "motor", "dm3510"), 2);
    assert_eq!(doc::reference_count(&s, &d, "motor", "dm4310"), 0);
    // `base` is a downloaded profile on both axes, plus s1's default.
    assert_eq!(doc::reference_count(&s, &d, "profile", "base"), 3);
    assert_eq!(doc::reference_count(&s, &d, "profile", "hold"), 2);

    let ids = doc::entry_ids(&d, "profile");
    assert_eq!(ids, vec!["base", "compliant", "fast", "hold"]);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn the_reference_document_is_clean() {
    let (s, d) = (schema(), document());
    let findings = doc::validate(&s, &d);
    assert!(
        errors(&findings).is_empty(),
        "unexpected errors: {:#?}",
        errors(&findings)
    );
}

#[test]
fn dangling_reference_is_an_error() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][0]["motor"]["ref"] = json!("dm9999");
    let f = doc::validate(&s, &d);
    let e = errors(&f);
    assert!(
        e.iter()
            .any(|f| f.path == "axis/0/motor" && f.message.contains("dm9999")),
        "{e:#?}"
    );
}

#[test]
fn out_of_range_value_is_an_error_and_names_the_field() {
    let (s, mut d) = (schema(), document());
    d["libraries"]["profile"]["base"]["vel_filter_shift"] = json!(12); // max 8
    let f = doc::validate(&s, &d);
    assert!(errors(&f)
        .iter()
        .any(|f| f.path == "libraries/profile/base/vel_filter_shift"));
}

#[test]
fn a_value_outside_the_proven_band_warns_without_blocking() {
    let (s, mut d) = (schema(), document());
    // Legal (range tops out at 1e6) but past what the bench has held.
    d["libraries"]["profile"]["base"]["vel_kp_ua_per_kcps"] = json!(1200.0);
    let f = doc::validate(&s, &d);
    assert!(errors(&f).is_empty(), "must not block: {:#?}", errors(&f));
    assert!(f
        .iter()
        .any(|f| f.severity == Severity::Warn && f.path.ends_with("vel_kp_ua_per_kcps")));
}

#[test]
fn wrong_scalar_type_is_an_error() {
    let (s, mut d) = (schema(), document());
    d["libraries"]["encoder"]["abz1024"]["lines"] = json!("1024");
    let f = doc::validate(&s, &d);
    assert!(errors(&f)
        .iter()
        .any(|f| f.path == "libraries/encoder/abz1024/lines"));
}

#[test]
fn unknown_keys_are_preserved_rather_than_rejected() {
    let (s, mut d) = (schema(), document());
    // Written by a newer schema this binary has never seen.
    d["libraries"]["motor"]["dm3510"]["thermal_tau_s"] = json!(45.0);
    let f = doc::validate(&s, &d);
    assert!(errors(&f).is_empty(), "{:#?}", errors(&f));
    assert_eq!(d["libraries"]["motor"]["dm3510"]["thermal_tau_s"], json!(45.0));
}

#[test]
fn a_nameplate_slot_refuses_local_overrides() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][0]["driver"]["overrides"] = json!({ "pwm_khz": 60.0 });
    let f = doc::validate(&s, &d);
    assert!(errors(&f)
        .iter()
        .any(|f| f.path == "axis/0/driver/overrides" && f.message.contains("铭牌")));
}

#[test]
fn a_slot_without_allow_override_refuses_them_too() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][0]["gearbox"]["overrides"] = json!({ "ratio_num": 50 });
    let f = doc::validate(&s, &d);
    assert!(errors(&f)
        .iter()
        .any(|f| f.path == "axis/0/gearbox/overrides"));
}

#[test]
fn inheritance_cycle_is_caught_and_does_not_hang() {
    let (s, mut d) = (schema(), document());
    d["libraries"]["profile"]["base"]
        .as_object_mut()
        .unwrap()
        .remove("stall_detect_ms");
    d["libraries"]["profile"]["base"]["extends"] = json!("hold");

    let f = doc::validate(&s, &d);
    assert!(errors(&f).iter().any(|f| f.message.contains("成环")));

    // Resolution must terminate on a field nothing in the loop writes, and
    // degrade to the declared default rather than spinning.
    let hold = &d["nodes"]["axis"][0]["profiles"][1];
    let r = doc::resolve_slot_field(&s, &d, "profile", Some(hold), "stall_detect_ms");
    assert_eq!(r.value, Some(ParamValue::U32(150)));
    assert_eq!(r.provenance, Provenance::Default);
}

#[test]
fn collection_bounds_are_enforced() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][0]["profiles"] = json!([]);
    let f = doc::validate(&s, &d);
    assert!(errors(&f).iter().any(|f| f.path == "axis/0/profiles"));
}

// ---------------------------------------------------------------------------
// Rules — the constraints no single field's range can express
// ---------------------------------------------------------------------------

#[test]
fn profile_speed_cap_is_checked_against_this_axis_gearbox() {
    let (s, mut d) = (schema(), document());
    // 40000 rpm is inside the field's own range but past what a pg08m takes.
    d["nodes"]["axis"][0]["profiles"][0]["overrides"] = json!({ "max_speed_rpm": 40000 });
    let f = doc::validate(&s, &d);
    let e = errors(&f);
    assert!(
        e.iter().any(|f| f.rule.as_deref() == Some("speed_within_gearbox")
            && f.path == "axis/0/profiles/0/max_speed_rpm"
            && f.message.contains("40000")
            && f.message.contains("25000")),
        "{e:#?}"
    );
}

#[test]
fn a_rule_is_evaluated_per_axis_not_across_axes() {
    let (s, mut d) = (schema(), document());
    // s1's motor is overridden to 540 mA; its base profile already limits the
    // velocity loop to 540. Raising only s0's limit must not implicate s1.
    d["nodes"]["axis"][0]["profiles"][0]["overrides"] = json!({ "vel_out_limit_ma": 700 });
    let f = doc::validate(&s, &d);
    let paths: Vec<&str> = errors(&f)
        .iter()
        .filter(|f| f.rule.as_deref() == Some("vel_limit_within_rating"))
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(paths, vec!["axis/0/profiles/0/vel_out_limit_ma"]);
}

#[test]
fn calibration_abort_must_stay_below_the_trip_level() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][0]["calibration"]["abort_current_ma"] = json!(900); // trip is 770
    let f = doc::validate(&s, &d);
    assert!(errors(&f)
        .iter()
        .any(|f| f.rule.as_deref() == Some("abort_below_trip")));
}

#[test]
fn slave_index_must_be_dense() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][1]["identity"]["index"] = json!(5);
    let f = doc::validate(&s, &d);
    let e = errors(&f);
    assert!(
        e.iter().any(|f| f.rule.as_deref() == Some("axis_index_dense")
            && f.path == "axis/1/identity/index"),
        "{e:#?}"
    );
}

#[test]
fn axis_names_must_be_unique() {
    let (s, mut d) = (schema(), document());
    d["nodes"]["axis"][1]["identity"]["name"] = json!("s0");
    let f = doc::validate(&s, &d);
    assert!(errors(&f)
        .iter()
        .any(|f| f.rule.as_deref() == Some("axis_name_unique")));
}

#[test]
fn the_default_profile_must_be_one_this_axis_downloads() {
    let (s, mut d) = (schema(), document());
    // `fast` exists in the library but is not among s0's downloaded groups,
    // so selecting it by ordinal at runtime would land on something else.
    d["nodes"]["axis"][0]["default_profile"]["ref"] = json!("fast");
    let f = doc::validate(&s, &d);
    let e = errors(&f);
    assert!(
        e.iter().any(|f| f.rule.as_deref() == Some("default_profile_downloaded")
            && f.path == "axis/0/default_profile"),
        "{e:#?}"
    );
}

// ---------------------------------------------------------------------------
// Fingerprint
// ---------------------------------------------------------------------------

#[test]
fn fingerprint_tracks_effective_values_not_file_bytes() {
    let (s, d) = (schema(), document());
    let base = doc::fingerprint(&s, &d);
    assert_eq!(base, doc::fingerprint(&s, &document()), "must be stable");

    // A comment or key-order change would not move it; an effective value does.
    let mut edited = document();
    edited["libraries"]["profile"]["base"]["pos_kp_per_s"] = json!(35.0);
    assert_ne!(base, doc::fingerprint(&s, &edited));

    // Editing a library entry nothing references must not move it either —
    // what a device runs on is what the fingerprint describes.
    let mut untouched = document();
    untouched["libraries"]["motor"]["dm4310"]["kt_nm_per_a"] = json!(0.0099);
    assert_eq!(base, doc::fingerprint(&s, &untouched));
}

// ---------------------------------------------------------------------------
// visible_when, in a document context
// ---------------------------------------------------------------------------

#[test]
fn visible_when_reads_resolved_siblings() {
    let s = schema();
    let cal = s.group("calibration").unwrap();
    let resolved = doc::resolve_group(cal, None);
    // No field in the fixture gates on another, so everything is relevant;
    // the point of the assertion is that an absent gate never hides a knob.
    assert!(cal
        .fields
        .iter()
        .all(|f| doc::field_visible(f, &resolved)));
}

// ---------------------------------------------------------------------------
// Edit targets — the "edit the type, or override it here" choice
// ---------------------------------------------------------------------------

#[test]
fn a_path_decides_where_an_edit_lands() {
    use sigflow_types::doc::{resolve_target, EditTarget};
    let s = schema();

    // Same field, two destinations. This is the whole of the top-bar switch.
    assert_eq!(
        resolve_target(&s, "libraries/motor/dm3510"),
        Some(EditTarget::LibraryEntry {
            library: "motor".into(),
            entry: "dm3510".into()
        })
    );
    assert_eq!(
        resolve_target(&s, "axis/0/motor"),
        Some(EditTarget::SlotOverride {
            library: "motor".into(),
            allow_override: true,
            nameplate: false
        })
    );

    // A group node owns its fields outright — no library, no override.
    assert_eq!(
        resolve_target(&s, "axis/1/travel"),
        Some(EditTarget::Group {
            group: "travel".into()
        })
    );
    assert_eq!(
        resolve_target(&s, "bus"),
        Some(EditTarget::Group {
            group: "bus".into()
        })
    );

    // The declarations that refuse an override travel with the target, so the
    // decision is made once, in the schema.
    assert_eq!(
        resolve_target(&s, "axis/0/gearbox"),
        Some(EditTarget::SlotOverride {
            library: "gearbox".into(),
            allow_override: false,
            nameplate: false
        })
    );
    assert!(matches!(
        resolve_target(&s, "axis/0/driver"),
        Some(EditTarget::SlotOverride { nameplate: true, .. })
    ));

    // Items of a reference collection are slots like any other.
    assert_eq!(
        resolve_target(&s, "axis/0/profiles/2"),
        Some(EditTarget::SlotOverride {
            library: "profile".into(),
            allow_override: true,
            nameplate: false
        })
    );

    // An instance is a sub-tree, not a field set; nothing is written to it.
    assert_eq!(resolve_target(&s, "axis/0"), None);
    assert_eq!(resolve_target(&s, "nope/0/motor"), None);
}

#[test]
fn target_group_names_the_fields_a_path_accepts() {
    use sigflow_types::doc::{resolve_target, target_group};
    let s = schema();
    let t = resolve_target(&s, "axis/0/motor").unwrap();
    let g = target_group(&s, &t).unwrap();
    assert_eq!(g.id, "motor");
    assert!(g.field("pole_pairs").is_some());
}

#[test]
fn a_scoped_rule_can_reach_the_document_root() {
    let (s, mut d) = (schema(), document());
    // Clean: 0.30 calibration modulation under the 0.49 bus ceiling.
    assert!(!doc::validate(&s, &d)
        .iter()
        .any(|f| f.rule.as_deref() == Some("cal_modulation_within_bus")));

    // A per-axis rule routinely needs something that is not per-axis. Without
    // a way to name it, the rule would silently never fire — which is worse
    // than not having written it.
    d["nodes"]["axis"][0]["calibration"]["modulation_max"] = json!(0.60);
    let f = doc::validate(&s, &d);
    let hit: Vec<&doc::Finding> = f
        .iter()
        .filter(|f| f.rule.as_deref() == Some("cal_modulation_within_bus"))
        .collect();
    assert_eq!(hit.len(), 1, "{f:#?}");
    assert_eq!(hit[0].path, "axis/0/calibration/modulation_max");
    assert_eq!(hit[0].severity, Severity::Warn);
    assert!(hit[0].message.contains("0.6") && hit[0].message.contains("0.49"), "{:?}", hit[0]);
}
