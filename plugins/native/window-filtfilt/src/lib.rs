//! sigflow.dsp.window_filtfilt — 通用窗口零相位滤波器。
//!
//! windowed capture 流（FLAG_DISCONTINUITY 开窗 + chunk 连续）→ 重组整窗
//! → 去直流 → 逐列 Butterworth filtfilt（scipy 语义，金标准夹具对齐）→
//! 每窗一帧 f64 交织输出，随帧注解透传。
//!
//! 窗口完成判定：capture-window 注解（capture 约定）优先；注解缺失退化为
//! "下一窗边界冲洗"。采样率：带内宣告（注解 fs）> 帧头推断——无参数；
//! 两者皆缺的首窗跳过（不用魔法默认值）。数值移植刻意保留 scipy 运算
//! 顺序，勿做"等价改写"。

pub mod filter;

use crate::filter::{butter, filtfilt, BType};
use sigflow_plugin_sdk::{
    decode_capture_window_annotation, parse_annotation, Frame, FrameOut, ParamValue, Plugin,
    PluginManifest, ProcessOutcome, CAPTURE_WINDOW_ANNOTATION_SCHEMA,
};

const F32: usize = 4;

pub struct WindowFiltfilt {
    // --- 参数 ---
    btype: BType,
    order: usize,
    fc_hz: f64,
    dc_offset: f64,
    verbose: bool,

    // --- fs 三级解析：带内宣告（capture-window 注解） > 帧头推断 > 先验 ---
    fs_declared: Option<f64>,
    prev_hdr: Option<(u64, i64)>,
    fs_est: f64,

    // --- 滤波器系数（缓存 + 失效条件）---
    design: Option<(f64, f64, usize, BType)>, // (fs, fc, order, btype) 已设计
    b: Vec<f64>,
    a: Vec<f64>,

    // --- 窗口重组 ---
    seg_open: bool,
    seg_buf: Vec<u8>,
    seg_cols: usize,
    seg_t0_ns: i64,
    seg_sample_index: u64,
    seg_gap: u64,
    /// 目标行数（capture-window 注解）；None = 边界冲洗模式。
    seg_target_rows: Option<usize>,
    /// 首 chunk 注解原文 [schema|body]，透传到整窗输出。
    seg_annotation: Option<Vec<u8>>,

    warned_gap: bool,
    warned_cap: bool,
    warned_nolen: bool,
    windows: u64,
    dropped: u64,
}

impl WindowFiltfilt {
    /// fs 解析：带内宣告 > 帧头推断。None = 尚无任何来源（首窗且注解无
    /// fs 的退化场合）——宁可跳过也不用魔法默认值。
    pub fn resolved_fs(&self) -> Option<f64> {
        self.fs_declared.or((self.fs_est > 0.0).then_some(self.fs_est))
    }

    /// 返回 false = fs 尚不可解析，本窗跳过。
    fn ensure_design(&mut self) -> bool {
        let Some(fs) = self.resolved_fs() else {
            return false;
        };
        let want = (fs, self.fc_hz, self.order, self.btype);
        if self.design == Some(want) {
            return true;
        }
        let wn = self.fc_hz / (want.0 / 2.0);
        if !(0.0..1.0).contains(&wn) {
            // 截止 ≥ Nyquist：保持旧系数（若有），一次性告警语义并入 verbose
            eprintln!(
                "window_filtfilt: fc {} Hz not below Nyquist ({} Hz) — keeping previous design",
                self.fc_hz,
                want.0 / 2.0
            );
            return !self.b.is_empty();
        }
        let (b, a) = butter(self.order, wn, self.btype);
        self.b = b;
        self.a = a;
        self.design = Some(want);
        true
    }

    fn reset_segment(&mut self) {
        self.seg_open = false;
        self.seg_buf.clear();
        self.seg_cols = 0;
        self.seg_t0_ns = 0;
        self.seg_sample_index = 0;
        self.seg_gap = 0;
        self.seg_target_rows = None;
        self.seg_annotation = None;
    }

    fn seg_rows(&self) -> usize {
        let row = self.seg_cols * F32;
        if row == 0 {
            0
        } else {
            self.seg_buf.len() / row
        }
    }

    /// 整窗滤波并发帧。段状态由本函数取走并重置。
    fn emit_window(&mut self, out: &mut FrameOut) {
        let rows = self.seg_rows();
        let cols = self.seg_cols;
        let arr: Vec<f64> = self.seg_buf[..rows * cols * F32]
            .chunks_exact(F32)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
            .collect();
        let (seg_t0, seg_index, pre_gap) = (self.seg_t0_ns, self.seg_sample_index, self.seg_gap);
        let annotation = self.seg_annotation.take();
        self.reset_segment();

        if !self.ensure_design() || self.b.is_empty() {
            // 首窗且无任何 fs 来源（注解无 fs + 单帧无推断对）：跳过比
            // 用魔法默认值诚实——正常 capture 链注解恒带 fs，走不到这里。
            eprintln!("window_filtfilt: sample rate not yet resolvable — window skipped");
            return;
        }
        let need = rows * cols * 8 + annotation.as_ref().map_or(0, Vec::len);
        if need > out.capacity() {
            if !self.warned_cap {
                self.warned_cap = true;
                eprintln!(
                    "window_filtfilt: filtered window {need} B exceeds output buffer {} B — windows skipped",
                    out.capacity()
                );
            }
            return;
        }

        let dc = self.dc_offset;
        let padlen = 3 * (self.a.len().max(self.b.len()) - 1);
        let mut cols_out: Vec<Vec<f64>> = Vec::with_capacity(cols);
        for c in 0..cols {
            let sig: Vec<f64> = (0..rows).map(|r| arr[r * cols + c] - dc).collect();
            match filtfilt(&self.b, &self.a, &sig, padlen) {
                Ok(f) => cols_out.push(f),
                Err(e) => {
                    eprintln!("window_filtfilt: filtfilt failed for window @sample {seg_index}: {e}");
                    return;
                }
            }
        }

        self.windows += 1;
        let dst = out.buffer_mut();
        for r in 0..rows {
            for (c, col) in cols_out.iter().enumerate() {
                let d = (r * cols + c) * 8;
                dst[d..d + 8].copy_from_slice(&col[r].to_le_bytes());
            }
        }
        out.set_written(rows * cols * 8);
        out.header.seq = self.windows;
        out.header.sample_index = seg_index;
        out.header.t0_ns = seg_t0;
        out.header.n_samples = rows as u32;
        out.header.flags = 0;
        out.header.set_discontinuity(pre_gap);
        if let Some((schema, body)) = annotation.as_deref().and_then(parse_annotation) {
            out.set_annotation(schema, body);
        }
        if self.verbose {
            eprintln!(
                "window_filtfilt: window #{} @sample {seg_index} rows={rows} cols={cols} fs={:.2}",
                self.windows,
                self.resolved_fs().unwrap_or(0.0)
            );
        }
    }

    fn ingest(&mut self, frame: &Frame, out: &mut FrameOut) {
        let h = frame.header;
        let data = frame.samples();
        if data.is_empty() || h.n_samples == 0 {
            return;
        }
        let cols = (data.len() / F32) / h.n_samples as usize;
        if cols == 0 {
            return;
        }

        // fs 推断（帧头自锚定轴；capture 流跨窗保持连贯样本轴）
        if let Some((pidx, pt0)) = self.prev_hdr {
            if h.sample_index > pidx && h.t0_ns > pt0 {
                let cand = (h.sample_index - pidx) as f64 * 1e9 / (h.t0_ns - pt0) as f64;
                if (1.0..1e8).contains(&cand) {
                    self.fs_est = cand;
                }
            }
        }
        self.prev_hdr = Some((h.sample_index, h.t0_ns));

        if h.is_discontinuity() {
            if self.seg_open && self.seg_rows() > 0 {
                if self.seg_target_rows.is_some() {
                    // 注解声明的行数没收满就被顶掉 → 缺 chunk，丢弃
                    self.dropped += 1;
                    eprintln!(
                        "window_filtfilt: incomplete window dropped ({}/{:?} rows)",
                        self.seg_rows(),
                        self.seg_target_rows
                    );
                    self.reset_segment();
                } else {
                    // 边界冲洗模式：上一窗到此完整
                    self.emit_window(out);
                }
            } else {
                self.reset_segment();
            }
            self.seg_open = true;
            self.seg_cols = cols;
            self.seg_t0_ns = h.t0_ns;
            self.seg_sample_index = h.sample_index;
            self.seg_gap = h.gap_samples;
            // capture-window 注解：目标行数 + 原文留作透传
            if let Some(blob) = frame.annotation() {
                self.seg_annotation = Some(blob.to_vec());
                if let Some((rows, _trig, fs)) = parse_annotation(blob)
                    .filter(|(s, _)| *s == CAPTURE_WINDOW_ANNOTATION_SCHEMA)
                    .and_then(|(_, body)| decode_capture_window_annotation(body))
                {
                    self.seg_target_rows = Some(rows as usize);
                    if fs.is_some() {
                        self.fs_declared = fs; // 源侧宣告：解析顺序最高级
                    }
                }
            }
            if self.seg_target_rows.is_none() && !self.warned_nolen {
                self.warned_nolen = true;
                eprintln!(
                    "window_filtfilt: no capture-window annotation — windows complete on the NEXT boundary (one-window latency)"
                );
            }
        } else if !self.seg_open {
            return; // 窗中途加入：等下一边界
        }

        // 段内连续性
        let rows_now = self.seg_rows();
        if rows_now > 0 {
            let expected = self.seg_sample_index + rows_now as u64;
            if h.sample_index != expected {
                if !self.warned_gap {
                    self.warned_gap = true;
                    eprintln!(
                        "window_filtfilt: in-window hole (expected sample_index {expected}, got {}) — window dropped; further holes not logged",
                        h.sample_index
                    );
                }
                self.dropped += 1;
                self.reset_segment();
                return;
            }
        }

        let row = self.seg_cols * F32;
        let usable = (data.len() / row) * row;
        self.seg_buf.extend_from_slice(&data[..usable]);

        if let Some(target) = self.seg_target_rows {
            if self.seg_rows() >= target {
                self.emit_window(out);
            }
        }
    }
}

impl Plugin for WindowFiltfilt {
    fn new(_manifest: &PluginManifest) -> Self {
        WindowFiltfilt {
            btype: BType::Highpass,
            order: 4,
            fc_hz: 100.0,
            dc_offset: 0.0,
            verbose: false,
            fs_declared: None,
            prev_hdr: None,
            fs_est: 0.0,
            design: None,
            b: Vec::new(),
            a: Vec::new(),
            seg_open: false,
            seg_buf: Vec::new(),
            seg_cols: 0,
            seg_t0_ns: 0,
            seg_sample_index: 0,
            seg_gap: 0,
            seg_target_rows: None,
            seg_annotation: None,
            warned_gap: false,
            warned_cap: false,
            warned_nolen: false,
            windows: 0,
            dropped: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("btype", ParamValue::Enum(v)) | ("btype", ParamValue::String(v)) => {
                self.btype = if v == "lowpass" { BType::Lowpass } else { BType::Highpass };
            }
            ("order", ParamValue::U32(v)) => self.order = (*v as usize).clamp(1, 8),
            ("fc_hz", ParamValue::F64(v)) => self.fc_hz = *v,
            ("dc_offset", ParamValue::F64(v)) => self.dc_offset = *v,
            ("verbose", ParamValue::Bool(v)) => self.verbose = *v,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.reset_segment();
        self.prev_hdr = None;
        self.fs_est = 0.0;
        self.fs_declared = None;
        ProcessOutcome::Ok
    }

    fn stop(&mut self) {
        self.reset_segment();
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let Some(out) = outputs.first_mut() else {
            return ProcessOutcome::Ok;
        };
        out.set_written(0);
        if let Some(frame) = inputs.first() {
            self.ingest(frame, out);
        }
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset" {
            self.reset_segment();
            self.prev_hdr = None;
            self.fs_est = 0.0;
        }
    }
}

#[cfg(feature = "abi")]
sigflow_plugin_sdk::export_plugin!(WindowFiltfilt);
