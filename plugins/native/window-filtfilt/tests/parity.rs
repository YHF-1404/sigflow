//! 金标准 parity（window_filtfilt 级）：butter 设计 + filtfilt 与 scipy
//! 逐位对齐；插件层窗口重组/注解透传/边界冲洗/撕洞语义。
//! 夹具见 testdata/knock/。

use sigflow_plugin_sdk::{
    encode_capture_window_annotation, parse_annotation, Frame, FrameHeader, FrameOut, ParamValue,
    Plugin, PluginManifest, ProcessOutcome, CAPTURE_WINDOW_ANNOTATION_SCHEMA, FLAG_ANNOTATED,
    FLAG_DISCONTINUITY,
};
use sigflow_plugin_window_filtfilt::filter::{butter, filtfilt, BType};
use sigflow_plugin_window_filtfilt::WindowFiltfilt;
use std::path::Path;

struct Fx {
    dir: std::path::PathBuf,
    man: serde_json::Value,
}

impl Fx {
    fn load() -> Option<Fx> {
        // 金标准夹具在私有 monorepo 的 testdata/knock（公共独立克隆不带）；
        // 找不到时测试优雅跳过而不是失败。
        let base = Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = ["../../../testdata/knock", "../../../../testdata/knock"]
            .iter()
            .map(|c| base.join(c))
            .find(|d| d.join("manifest.json").exists())?;
        let man = serde_json::from_str(
            &std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        Some(Fx { dir, man })
    }

    fn bytes(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.join(self.man["arrays"][name]["file"].as_str().unwrap())).unwrap()
    }

    fn f64s(&self, name: &str) -> Vec<f64> {
        let e = &self.man["arrays"][name];
        let bytes = self.bytes(name);
        match e["dtype"].as_str().unwrap() {
            "<f8" => bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect(),
            "<f4" => bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
            d => panic!("unsupported dtype {d}"),
        }
    }

    fn scalar(&self, name: &str) -> f64 {
        self.man["scalars"][name].as_f64().unwrap()
    }

    fn param(&self, name: &str) -> f64 {
        self.man["params"][name].as_f64().unwrap()
    }

    /// 从 7 列夹具窗抽两列（ch_s3, ch_s4），拼 2 列交织 f32（保留直流）。
    fn two_col_window(&self) -> (Vec<u8>, usize) {
        let raw = self.bytes("cap0_window");
        let nch = self.man["arrays"]["cap0_window"]["shape"][1].as_u64().unwrap() as usize;
        let rows = self.man["arrays"]["cap0_window"]["shape"][0].as_u64().unwrap() as usize;
        let (c3, c4) = (self.param("ch_s3") as usize, self.param("ch_s4") as usize);
        let f = |r: usize, c: usize| &raw[(r * nch + c) * 4..(r * nch + c) * 4 + 4];
        let mut out = Vec::with_capacity(rows * 8);
        for r in 0..rows {
            out.extend_from_slice(f(r, c3));
            out.extend_from_slice(f(r, c4));
        }
        (out, rows)
    }
}

fn assert_close(label: &str, got: &[f64], want: &[f64], rtol: f64) {
    assert_eq!(got.len(), want.len(), "{label}: length {} vs {}", got.len(), want.len());
    let scale = want.iter().fold(0.0f64, |m, v| m.max(v.abs())).max(1e-30);
    let (mut worst, mut wi) = (0.0f64, 0usize);
    for i in 0..got.len() {
        let d = (got[i] - want[i]).abs();
        if d > worst {
            worst = d;
            wi = i;
        }
    }
    assert!(
        worst <= rtol * scale,
        "{label}: max diff {worst:.3e} @[{wi}] (got {} want {}), tol {:.3e}",
        got[wi], want[wi], rtol * scale
    );
}

#[test]
fn butter_highpass_matches_scipy() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let wn = fx.param("fc_hp_hz") / (fx.param("fs_hz") / 2.0);
    let (b, a) = butter(4, wn, BType::Highpass);
    assert_close("butter b", &b, &fx.f64s("butter_b"), 1e-13);
    assert_close("butter a", &a, &fx.f64s("butter_a"), 1e-13);
}

/// 低通走同一套 zpk 机制：设计性质守护（无 scipy 夹具）。
#[test]
fn butter_lowpass_design_is_sane() {
    for order in [1usize, 2, 4, 8] {
        let (b, a) = butter(order, 0.2, BType::Lowpass);
        assert_eq!(b.len(), order + 1);
        assert_eq!(a.len(), order + 1);
        // DC 增益 = 1
        let dc = b.iter().sum::<f64>() / a.iter().sum::<f64>();
        assert!((dc - 1.0).abs() < 1e-10, "order {order}: DC gain {dc}");
        // Nyquist 增益 ≈ 0（z=-1 处 order 重零点）
        let alt = |v: &[f64]| v.iter().enumerate().map(|(i, x)| x * (-1.0f64).powi(i as i32)).sum::<f64>();
        assert!(
            (alt(&b) / alt(&a)).abs() < 1e-9,
            "order {order}: Nyquist gain {}",
            alt(&b) / alt(&a)
        );
    }
    // 高通对偶性质
    let (b, a) = butter(4, 0.2, BType::Highpass);
    assert!((b.iter().sum::<f64>() / a.iter().sum::<f64>()).abs() < 1e-10, "highpass DC gain");
}

#[test]
fn filtfilt_matches_scipy() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let (b, a) = (fx.f64s("butter_b"), fx.f64s("butter_a"));
    let padlen = 3 * (a.len().max(b.len()) - 1);
    let raw = fx.f64s("cap0_window");
    let nch = fx.man["arrays"]["cap0_window"]["shape"][1].as_u64().unwrap() as usize;
    let dc = fx.param("dc_offset");
    let col = |c: usize| -> Vec<f64> { (0..raw.len() / nch).map(|r| raw[r * nch + c] - dc).collect() };
    let got3 = filtfilt(&b, &a, &col(fx.param("ch_s3") as usize), padlen).unwrap();
    let got4 = filtfilt(&b, &a, &col(fx.param("ch_s4") as usize), padlen).unwrap();
    assert_close("s3f", &got3, &fx.f64s("cap0_s3f"), 1e-10);
    assert_close("s4f", &got4, &fx.f64s("cap0_s4f"), 1e-10);
}

// ---- 插件层 ----

fn manifest() -> PluginManifest {
    toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
}

fn deployed_plugin(fx: &Fx) -> WindowFiltfilt {
    let mut p = WindowFiltfilt::new(&manifest());
    p.set_param("fc_hz", &ParamValue::F64(fx.param("fc_hp_hz")));
    p.set_param("dc_offset", &ParamValue::F64(fx.param("dc_offset")));
    p
}

/// 头 + capture-window 注解拼进 payload（模拟 trigger-capture 的首 chunk）。
fn annotated(payload: &[u8], rows: u64, trig: u64) -> (Vec<u8>, u32) {
    annotated_fs(payload, rows, trig, None)
}

fn annotated_fs(payload: &[u8], rows: u64, trig: u64, fs: Option<f64>) -> (Vec<u8>, u32) {
    let body = encode_capture_window_annotation(rows, trig, fs);
    let mut out = payload.to_vec();
    out.extend_from_slice(&CAPTURE_WINDOW_ANNOTATION_SCHEMA.to_le_bytes());
    out.extend_from_slice(&body);
    (out, (8 + body.len()) as u32)
}

fn header(idx: u64, t0: i64, n: usize, disc: bool, annot_len: u32) -> FrameHeader {
    let mut h = FrameHeader::ZERO;
    h.seq = 1;
    h.sample_index = idx;
    h.t0_ns = t0;
    h.n_samples = n as u32;
    if disc {
        h.flags |= FLAG_DISCONTINUITY;
    }
    if annot_len > 0 {
        h.flags |= FLAG_ANNOTATED;
        h.annotation_len = annot_len;
    }
    h
}

fn run(p: &mut WindowFiltfilt, h: FrameHeader, data: &[u8]) -> Option<(FrameHeader, Vec<u8>)> {
    let mut buf = vec![0u8; 1024 * 1024];
    let (w, oh) = {
        let mut outs = vec![FrameOut::new(&mut buf)];
        let frames = [Frame::new(h, data)];
        assert!(matches!(p.process(&frames, &mut outs), ProcessOutcome::Ok));
        (outs[0].written(), outs[0].header)
    };
    (w > 0).then(|| (oh, buf[..w].to_vec()))
}

/// 注解定窗长：整窗单帧 → f64 输出对夹具、注解透传、帧头语义。
#[test]
fn annotated_window_filters_and_passes_annotation() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let mut p = deployed_plugin(&fx);
    p.start();
    let (win, rows) = fx.two_col_window();
    let trig = fx.scalar("cap0_trig") as u64;
    let (data, alen) = annotated_fs(&win, rows as u64, trig, Some(fx.param("fs_hz")));
    let (oh, out) = run(&mut p, header(500, 9_000_000_000, rows, true, alen), &data)
        .expect("window emitted immediately (annotation gives the length)");
    assert_eq!(oh.sample_index, 500);
    assert_eq!(oh.t0_ns, 9_000_000_000);
    assert_eq!(oh.n_samples as usize, rows);
    assert!(oh.flags & FLAG_DISCONTINUITY != 0);
    // 注解透传
    let of = Frame::new(oh, &out);
    let (schema, body) = parse_annotation(of.annotation().expect("annotated")).unwrap();
    assert_eq!(schema, CAPTURE_WINDOW_ANNOTATION_SCHEMA);
    assert_eq!(
        sigflow_plugin_sdk::decode_capture_window_annotation(body),
        Some((rows as u64, trig, Some(fx.param("fs_hz"))))
    );
    // payload = filtfilt(dc-removed)：对夹具（自设计系数 → 1e-9 档）
    let samples = of.samples();
    assert_eq!(samples.len(), rows * 16);
    let mut s3f = Vec::with_capacity(rows);
    let mut s4f = Vec::with_capacity(rows);
    for r in 0..rows {
        s3f.push(f64::from_le_bytes(samples[r * 16..r * 16 + 8].try_into().unwrap()));
        s4f.push(f64::from_le_bytes(samples[r * 16 + 8..r * 16 + 16].try_into().unwrap()));
    }
    assert_close("payload s3f", &s3f, &fx.f64s("cap0_s3f"), 1e-9);
    assert_close("payload s4f", &s4f, &fx.f64s("cap0_s4f"), 1e-9);
}

/// 注解宣告的 fs 优先于先验与推断（三级解析最高级）。
#[test]
fn declared_fs_wins_over_prior_and_inference() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let mut p = deployed_plugin(&fx); // 先验 = 64000
    p.start();
    let (win, rows) = fx.two_col_window();
    let (data, alen) = annotated_fs(&win, rows as u64, 614, Some(32000.0));
    run(&mut p, header(0, 1_000, rows, true, alen), &data).expect("window emitted");
    assert_eq!(p.resolved_fs(), Some(32000.0), "annotation fs must win");
}

/// 无注解 → 边界冲洗模式：第二个窗边界到达时冲洗第一窗；此时帧头推断
/// 已就位（无 fs 参数的前提正是这个时序）。
#[test]
fn boundary_flush_without_annotation() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let fs = fx.param("fs_hz");
    let t0_of = |idx: u64| (idx as f64 / fs * 1e9) as i64;
    let mut p = deployed_plugin(&fx);
    p.start();
    let (win, rows) = fx.two_col_window();
    assert!(run(&mut p, header(0, t0_of(0), rows, true, 0), &win).is_none(), "no length info yet");
    let (oh, _) = run(&mut p, header(100_000, t0_of(100_000), rows, true, 0), &win)
        .expect("previous window flushed on the next boundary (fs inferred by then)");
    assert_eq!(oh.sample_index, 0, "flushed window is the FIRST one");
    assert_eq!(oh.n_samples as usize, rows);
    assert_eq!(p.resolved_fs(), Some(fs), "inference resolved at flush time");
}

/// 完全无 fs 来源（注解无 fs + 单帧首窗）：跳过并告警，不用魔法默认值。
#[test]
fn unresolvable_fs_skips_window() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let mut p = deployed_plugin(&fx);
    p.start();
    let (win, rows) = fx.two_col_window();
    let (data, alen) = annotated(&win, rows as u64, 614); // 注解无 fs
    assert!(
        run(&mut p, header(0, 1_000, rows, true, alen), &data).is_none(),
        "first window without any fs source must be skipped"
    );
}

/// 分 chunk 重组 + 撕洞丢弃恢复（注解定长模式）。
#[test]
fn chunked_reassembly_and_torn_window() {
    let Some(fx) = Fx::load() else {
        eprintln!("SKIP: knock fixtures not present (monorepo testdata/knock)");
        return;
    };
    let fs = fx.param("fs_hz");
    let (win, rows) = fx.two_col_window();
    let trig = fx.scalar("cap0_trig") as u64;
    let row_b = 8; // 2 列 f32
    let t0 = 1_000_000_000i64;

    let mut p = deployed_plugin(&fx);
    p.start();
    let mut emitted = 0;
    let mut off = 0usize;
    for (i, len) in [700usize, 700, rows - 1400].into_iter().enumerate() {
        let chunk = &win[off * row_b..(off + len) * row_b];
        let (data, alen) = if i == 0 {
            annotated_fs(chunk, rows as u64, trig, Some(fs))
        } else {
            (chunk.to_vec(), 0)
        };
        if run(&mut p, header(off as u64, t0 + off as i64, len, i == 0, alen), &data).is_some() {
            emitted += 1;
        }
        off += len;
    }
    assert_eq!(emitted, 1, "chunked window must emit once");

    let mut p = deployed_plugin(&fx);
    p.start();
    let (first, alen) = annotated_fs(&win[..700 * row_b], rows as u64, trig, Some(fs));
    assert!(run(&mut p, header(0, t0, 700, true, alen), &first).is_none());
    assert!(
        run(&mut p, header(1400, t0, rows - 1400, false, 0), &win[1400 * row_b..rows * row_b]).is_none(),
        "torn window must not emit"
    );
    let (full, alen) = annotated_fs(&win, rows as u64, trig, Some(fs));
    assert!(
        run(&mut p, header(90_000, t0 + 7, rows, true, alen), &full).is_some(),
        "fresh window recovers"
    );
}
