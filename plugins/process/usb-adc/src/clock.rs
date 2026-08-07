//! Device-clock recovery: estimate the true host-time-per-device-sample rate.
//!
//! The device free-runs on its own crystal (HSE), nominally 64000 Hz/ch but
//! actually off by a crystal offset (measured ~+107 ppm) that, if ignored,
//! makes `t0 = stream_t0 + sample_index / nominal_fs` drift ~tens of µs per
//! second. We periodically read the device's free-running sample counter
//! (`GET_DEVICE_TIME`) bracketed by the host monotonic clock, giving
//! observations `(device_sample_count, host_mono)`. The true rate is the slope
//! `b = dH/dS` (host ns per device sample); fitting it over a long baseline
//! averages out the per-observation round-trip jitter (~RTT/2).
//!
//! Only the *slope* matters for drift — the intercept (absolute phase) folds
//! into the one-time `stream_t0` anchor (a fixed ~100 µs–ms offset we accept;
//! tightening it needs per-packet device timestamps, deliberately deferred).
//!
//! Robustness: observations are only admitted when the *local* rate between
//! consecutive reads is near nominal. This rejects the startup ramp (the device
//! emits nothing for ~0.6 s then ramps to full rate over ~0.75 s, so early reads
//! have a near-flat sample_count that would otherwise poison the least-squares
//! slope) and any transient glitch.

use std::collections::VecDeque;

use crate::protocol::DeviceTimeObs;

/// Sliding window of observations (at ~100 ms each → ~25 s baseline).
const WINDOW: usize = 256;
/// Need at least this many before trusting the slope.
const MIN_OBS: usize = 16;
/// Drop observations whose control round trip was too jittery to be useful.
const RTT_MAX_NS: i64 = 2_000_000; // 2 ms
/// Admit a pair only if its local rate is within this fraction of nominal.
const RATE_TOLERANCE: f64 = 0.10;

pub struct ClockRecovery {
    /// (device_sample_count, host_mono_mid_ns), oldest at front — steady-state
    /// observations only.
    obs: VecDeque<(u64, i64)>,
    /// Most recent raw observation (for the local-rate gate); may be a rejected
    /// transient.
    prev: Option<(u64, i64)>,
}

impl ClockRecovery {
    pub fn new() -> Self {
        ClockRecovery {
            obs: VecDeque::with_capacity(WINDOW),
            prev: None,
        }
    }

    /// Fresh stream — forget the lock.
    pub fn reset(&mut self) {
        self.obs.clear();
        self.prev = None;
    }

    /// Feed one observation. `nominal_ns_per_sample` (= 1e9 / nominal_fs) gates
    /// out transients. Jittery round trips and backward counters are rejected.
    pub fn record(&mut self, obs: DeviceTimeObs, nominal_ns_per_sample: f64) {
        if obs.rtt_ns < 0 || obs.rtt_ns > RTT_MAX_NS {
            return; // jittery round trip — ignore, keep prev for the next gate
        }
        let cur = (obs.sample_count, obs.host_mono_mid_ns);
        let prev = self.prev.replace(cur);
        let (ps, ph) = match prev {
            Some(p) => p,
            None => return, // need a baseline to gate the next observation
        };
        if cur.0 < ps || cur.1 < ph {
            // Counter or clock went backwards (pipe reset) — restart the fit.
            self.obs.clear();
            return;
        }
        let dcount = (cur.0 - ps) as f64;
        let dhost = (cur.1 - ph) as f64;
        if dcount < 1.0 || dhost <= 0.0 {
            return;
        }
        // Steady-state gate: reject the startup ramp and glitches.
        let local = dhost / dcount; // ns per sample
        if (local - nominal_ns_per_sample).abs() > nominal_ns_per_sample * RATE_TOLERANCE {
            return;
        }
        if self.obs.is_empty() {
            self.obs.push_back((ps, ph)); // seed with the steady previous point
        }
        if self.obs.len() == WINDOW {
            self.obs.pop_front();
        }
        self.obs.push_back(cur);
    }

    /// Least-squares slope `dH/dS` = host ns per device sample, or `None` until
    /// enough observations span a usable baseline. Origin-shifted to the oldest
    /// observation to keep the sums small and precise.
    pub fn ns_per_sample(&self) -> Option<f64> {
        if self.obs.len() < MIN_OBS {
            return None;
        }
        let (s0, h0) = *self.obs.front().unwrap();
        let n = self.obs.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for &(s, h) in &self.obs {
            let x = (s - s0) as f64;
            let y = (h - h0) as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denom = n * sxx - sx * sx;
        if denom.abs() < 1.0 {
            return None; // degenerate: all samples at one count
        }
        Some((n * sxy - sx * sy) / denom)
    }

    #[cfg(test)]
    pub fn obs_len(&self) -> usize {
        self.obs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOMINAL: f64 = 1e9 / 64000.0;

    fn obs(sample_count: u64, host_mono_mid_ns: i64, rtt_ns: i64) -> DeviceTimeObs {
        DeviceTimeObs {
            sample_count,
            host_mono_mid_ns,
            rtt_ns,
        }
    }

    #[test]
    fn recovers_true_rate_not_nominal() {
        // Device truly runs at 64007 Hz/ch while nominal is 64000. Feed clean
        // steady observations and confirm the recovered slope matches the truth.
        let mut c = ClockRecovery::new();
        let true_ns_per_sample = 1e9 / 64007.0;
        for k in 0..64u64 {
            let s = 1_000_000 + k * 6400; // ~100 ms of samples between obs
            let h = 900_000_000_000 + (s as f64 * true_ns_per_sample) as i64;
            c.record(obs(s, h, 300_000), NOMINAL);
        }
        let recovered_hz = 1e9 / c.ns_per_sample().expect("locked");
        assert!((recovered_hz - 64007.0).abs() < 1.0, "got {recovered_hz}");
    }

    #[test]
    fn rejects_startup_ramp() {
        // Simulate ~0.6 s of near-flat counter (device not yet streaming) then
        // steady 64007 Hz. The ramp must not poison the slope.
        let mut c = ClockRecovery::new();
        let base_h = 900_000_000_000i64;
        // Ramp: host advances 100 ms/step, sample_count crawls (~near zero).
        for k in 0..6i64 {
            c.record(obs(10 + (k as u64) * 5, base_h + k * 100_000_000, 300_000), NOMINAL);
        }
        // Steady from t = 0.6 s.
        let true_ns = 1e9 / 64007.0;
        for k in 0..64u64 {
            let s = 40_000 + k * 6400;
            let h = base_h + 600_000_000 + (((s - 40_000) as f64) * true_ns) as i64;
            c.record(obs(s, h, 300_000), NOMINAL);
        }
        let recovered_hz = 1e9 / c.ns_per_sample().expect("locked");
        assert!((recovered_hz - 64007.0).abs() < 2.0, "ramp poisoned slope: {recovered_hz}");
    }

    #[test]
    fn none_until_enough_obs() {
        let mut c = ClockRecovery::new();
        let true_ns = 1e9 / 64000.0;
        for k in 0..(MIN_OBS as u64 - 2) {
            let s = 1000 + k * 6400;
            let h = 900_000_000_000 + (s as f64 * true_ns) as i64;
            c.record(obs(s, h, 300_000), NOMINAL);
        }
        assert!(c.ns_per_sample().is_none());
    }

    #[test]
    fn rejects_jittery_round_trips() {
        let mut c = ClockRecovery::new();
        let true_ns = 1e9 / 64000.0;
        // A jittery obs in the middle should be skipped, not admitted.
        for k in 0..32u64 {
            let s = 1_000_000 + k * 6400;
            let h = 900_000_000_000 + (s as f64 * true_ns) as i64;
            let rtt = if k == 10 { 9_000_000 } else { 300_000 };
            c.record(obs(s, h, rtt), NOMINAL);
        }
        // 32 reads, one jittery → 31 admitted (minus seeding bookkeeping).
        assert!(c.obs_len() >= 30 && c.obs_len() <= 31, "got {}", c.obs_len());
    }
}
