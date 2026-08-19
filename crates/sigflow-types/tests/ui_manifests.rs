//! Every shipped UI widget manifest must still parse into [`PluginManifest`].
//!
//! These are pure-manifest packages with no Rust of their own, so nothing else
//! would notice a manifest that the contract has outgrown — a widget only
//! fails at `plugin install` time, on someone else's machine.

use std::path::PathBuf;

use sigflow_types::manifest::{PluginManifest, Presentation};
use sigflow_types::plugin::PluginCategory;

fn ui_dir() -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "..", "..", "plugins", "ui"]
        .iter()
        .collect()
}

#[test]
fn every_widget_manifest_parses() {
    let mut seen = 0;
    for e in std::fs::read_dir(ui_dir()).expect("plugins/ui is readable") {
        let path = e.expect("dir entry").path().join("manifest.toml");
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read manifest");
        let m: PluginManifest =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(m.category, PluginCategory::UiWidget, "{}", path.display());
        assert!(m.widget.is_some(), "{} has no [widget]", path.display());
        seen += 1;
    }
    assert!(seen >= 10, "expected the shipped widget set, found {seen}");
}

#[test]
fn presentation_defaults_to_face_and_param_table_asks_for_a_page() {
    let read = |name: &str| -> PluginManifest {
        let p = ui_dir().join(name).join("manifest.toml");
        toml::from_str(&std::fs::read_to_string(&p).expect("read")).expect("parse")
    };

    // Every widget that predates `presentation` keeps rendering on the face.
    let slider = read("slider").widget.unwrap();
    assert_eq!(slider.presentation, Presentation::Face);

    let table = read("param-table").widget.unwrap();
    assert_eq!(table.presentation, Presentation::Page);
    assert_eq!(
        table.bindable_to.len(),
        1,
        "the param table binds documents and nothing else"
    );
    assert_eq!(table.bindable_to[0].kind.as_str(), "doc");
}
