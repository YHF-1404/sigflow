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

    // 手势表也是整页的，但绑的是一个**字符串参数**（手势库的 JSON），不是
    // 文档——它编的东西没有 DocSchema 那种覆盖链，是一个矩阵。
    let gesture = read("gesture-table").widget.unwrap();
    assert_eq!(gesture.presentation, Presentation::Page);
    assert_eq!(gesture.bindable_to.len(), 1);
    assert_eq!(gesture.bindable_to[0].kind.as_str(), "param");
    assert_eq!(
        gesture.bindable_to[0].data_type.as_deref(),
        Some("string"),
        "手势库是一份 JSON——绑到数值参数上这个控件读不出东西"
    );

    // 圆盘电机也是整页的，绑的是**生产口**（rotor，带列契约）；它往节点写的
    // 两个参数走 config（hold_param / angle_param），不是第二个绑定——绑定只有
    // 一个，这里钉住这一点。
    let disc = read("motor-disc").widget.unwrap();
    assert_eq!(disc.presentation, Presentation::Page);
    assert_eq!(disc.bindable_to.len(), 1);
    assert_eq!(disc.bindable_to[0].kind.as_str(), "port");
    for k in ["hold_param", "angle_param", "burst_param"] {
        assert!(
            disc.config_schema.contains_key(k),
            "交互写的参数名在 config 里改，不在绑定里改：缺 {k}"
        );
    }
}

#[test]
fn 极坐标盘是脸上的控件_绑口_旋钮名走_config() {
    // 盘面的倍率 / 偏置 / 余辉 / 两个模式是**绑定节点的参数**（控件之间不能互相驱动，滑块只能
    // 写参数）；控件配置里只说它们叫什么名字。六个 *_param 键缺一个，那个旋钮就换不了上游。
    let p = ui_dir().join("polar-trace").join("manifest.toml");
    let m: PluginManifest =
        toml::from_str(&std::fs::read_to_string(&p).expect("read")).expect("parse");
    let w = m.widget.expect("[widget]");
    assert_eq!(w.widget_type, "polar_trace");
    assert_eq!(w.presentation, Presentation::Face);
    assert_eq!(w.bindable_to.len(), 1);
    assert_eq!(w.bindable_to[0].kind.as_str(), "port");
    for k in [
        "r_gain_param", "diff_gain_param", "r_offset_param", "persist_param", "radius_mode_param",
        "deviation_param", "theta_column", "err_column", "gen_column", "groups",
    ] {
        assert!(w.config_schema.contains_key(k), "缺配置键 {k}");
    }
}

#[test]
fn 示波器是整页控件_绑口_配置只有_label() {
    // setup 全在 bind.scope（ScopeConfig），config 里只许有 label——旋钮不是控件配置。
    let p = ui_dir().join("oscilloscope").join("manifest.toml");
    let m: PluginManifest =
        toml::from_str(&std::fs::read_to_string(&p).expect("read")).expect("parse");
    let w = m.widget.expect("[widget]");
    assert_eq!(w.widget_type, "oscilloscope");
    assert_eq!(w.presentation, Presentation::Page);
    assert_eq!(w.bindable_to.len(), 1);
    assert_eq!(w.bindable_to[0].kind.as_str(), "port");
    let keys: Vec<&String> = w.config_schema.keys().collect();
    assert_eq!(keys, vec!["label"], "示波器的 config 只有 label，其余都是 setup");
}
