//! 列契约：`PortDescriptor.columns` 的 TOML 形状就是 [`ColumnDecl`] 文档里
//! 写的那个——这里把文档里的例子原样解析一遍，文档说谎就会红。

use sigflow_types::manifest::{BoundRef, ColumnKind, PluginManifest, PortDescriptor, Tone};

const MANIFEST: &str = r#"
name = "test.columns"
version = "0.1.0"
category = "compute"

[runtime]
type = "native"
library = "x.so"
entry = "e"

[[ports]]
id = "status"
direction = "producer"
semantic_type = { kind = "timeseries.signal", dtype = "f32" }
column_groups = { repeat_by = "slaves", label = "s{i}", title_columns = ["alias", "drive_mode"] }

[[ports.columns]]
id = "sw"
label = "状态字"
kind = "enum"
decode = [
  { mask = 0x6f, value = 0x27, name = "Operation enabled", tone = "good" },
  { mask = 0x6f, value = 0x23, name = "Switched on" },
  { mask = 0x4f, value = 0x08, name = "Fault", tone = "crit" },
]
bits = [ { bit = 11, name = "限幅", tone = "warn" }, { bit = 3, name = "故障", tone = "crit" } ]

[[ports.columns]]
id = "ferr"
label = "跟随误差"
unit = "counts"
kind = "bounded"
bound = { max = { column = "ferr_lim" }, bipolar = true }

[[ports.columns]]
id = "ferr_lim"

[[ports.columns]]
id = "margin"
kind = "bounded"
bound = { min = 0.0, max = 1000.0 }

[[ports.columns]]
id = "dropouts"
kind = "counter"

[[ports.columns]]
id = "alias"
label = "别名"

[[ports.columns]]
id = "drive_mode"
kind = "enum"
decode = [ { value = 1, name = "开环", tone = "warn" } ]

[[ports]]
id = "plain"
direction = "producer"
semantic_type = { kind = "timeseries.signal", dtype = "f32" }
"#;

fn col(c: &str) -> Option<BoundRef> {
    Some(BoundRef::Column { column: c.into() })
}

#[test]
fn the_documented_shape_parses_and_roundtrips() {
    let m: PluginManifest = toml::from_str(MANIFEST).expect("parse");
    let status = &m.ports[0];
    let groups = status.column_groups.as_ref().expect("groups");
    assert_eq!(groups.repeat_by.as_deref(), Some("slaves"));
    assert_eq!(groups.label.as_deref(), Some("s{i}"));
    assert_eq!(groups.title_columns, ["alias", "drive_mode"], "身份/模式列跟进组标题");
    assert_eq!(status.columns.len(), 7);

    // 模式列只声明非缺省态：value=0（foc）不命中任何 decode 条目 → 标题不加东西。
    let dm = &status.columns[6];
    assert_eq!(dm.decode.len(), 1);
    assert_eq!(dm.decode[0].mask, None, "缺省 mask = 整值相等");
    assert_eq!(dm.decode[0].tone, Tone::Warn);

    let sw = &status.columns[0];
    assert_eq!(sw.kind, ColumnKind::Enum);
    assert_eq!(sw.decode.len(), 3);
    assert_eq!(sw.decode[0].mask, Some(0x6f));
    assert_eq!(sw.decode[0].tone, Tone::Good);
    assert_eq!(sw.decode[1].tone, Tone::Neutral, "tone 缺省中性");
    assert_eq!(sw.bits[0].bit, 11);
    assert_eq!(sw.bits[0].tone, Tone::Warn);

    let ferr = &status.columns[1];
    let b = ferr.bound.as_ref().expect("bound");
    assert!(b.bipolar);
    assert_eq!(b.max, col("ferr_lim"), "界来自同组另一列");
    assert_eq!(b.min, None);

    // 没写 kind 的列是 signal——老 manifest 一个字不改就是这个意思。
    assert_eq!(status.columns[2].kind, ColumnKind::Signal);

    let margin = &status.columns[3];
    let mb = margin.bound.as_ref().unwrap();
    assert_eq!(mb.min, Some(BoundRef::Value(0.0)));
    assert_eq!(mb.max, Some(BoundRef::Value(1000.0)));
    assert!(!mb.bipolar);

    assert_eq!(status.columns[4].kind, ColumnKind::Counter);

    // 没声明列的口：空 Vec、None。columns 照样以 [] 出现——前端类型里它
    // 是必填，JSON 得和类型说一样的话；column_groups 是 Option，缺省不出现。
    let plain = &m.ports[1];
    assert!(plain.columns.is_empty());
    assert!(plain.column_groups.is_none());
    // title_columns 不写 = 空 Vec;序列化永远带 []——与 columns 同例,
    // 前端类型必填,JSON 得和类型说一样的话。老 manifest 零感知。
    let old: sigflow_types::manifest::ColumnGroups =
        toml::from_str(r#"repeat_by = "slaves""#).unwrap();
    assert!(old.title_columns.is_empty());
    assert_eq!(serde_json::to_value(&old).unwrap()["title_columns"], serde_json::json!([]));
    let json = serde_json::to_value(plain).unwrap();
    assert_eq!(json["columns"], serde_json::json!([]));
    assert!(json.get("column_groups").is_none());

    // JSON 往返：BoundRef 是 untagged，数值和 { column } 两种形状都要能回来。
    let json = serde_json::to_string(status).unwrap();
    let back: PortDescriptor = serde_json::from_str(&json).unwrap();
    assert_eq!(back.columns[1].bound.as_ref().unwrap().max, col("ferr_lim"));
    assert_eq!(back.columns[3].bound.as_ref().unwrap().max, Some(BoundRef::Value(1000.0)));
}
