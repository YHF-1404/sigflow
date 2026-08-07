//! Raw ADC sample → voltage conversion, and forward-fill for lost packets.
//!
//! The device sends interleaved little-endian `u16` samples (12-bit data in
//! 16-bit containers). The plugin converts each to an f32 voltage so the rest
//! of the graph sees a uniform `f32` multichannel stream:
//!
//! ```text
//! voltage = raw / ((1 << samplebits) - 1) * vref
//! ```

/// Convert one raw sample to a voltage.
pub fn raw_to_voltage(raw: u16, samplebits: u8, vref: f32) -> f32 {
    let full_scale = ((1u32 << samplebits.min(16)) - 1) as f32;
    if full_scale <= 0.0 {
        return 0.0;
    }
    raw as f32 / full_scale * vref
}

/// Convert an interleaved buffer of LE `u16` samples to interleaved `f32`
/// voltage. Writes into `out` (4 bytes per sample) and returns the number of
/// f32 samples written. Stops at whichever of `raw`/`out` runs out.
pub fn convert_block(raw: &[u8], out: &mut [u8], samplebits: u8, vref: f32) -> usize {
    let n = (raw.len() / 2).min(out.len() / 4);
    for i in 0..n {
        let s = u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]);
        let v = raw_to_voltage(s, samplebits, vref);
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    n
}

/// Forward-fill: copy the previous packet's bytes into `dst` to stand in for a
/// lost packet, keeping the sample stream contiguous and time-aligned. The
/// accompanying frame header still flags the discontinuity so downstream knows
/// the filled region is synthetic. Returns bytes filled.
///
/// (The ISO layer currently inlines this fill per-packet; kept here as the
/// documented, unit-tested reference for the forward-fill policy.)
#[allow(dead_code)]
pub fn forward_fill(prev: &[u8], dst: &mut [u8]) -> usize {
    let n = prev.len().min(dst.len());
    dst[..n].copy_from_slice(&prev[..n]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_endpoints_12bit() {
        // 12-bit full scale = 4095.
        assert!((raw_to_voltage(0, 12, 3.3) - 0.0).abs() < 1e-6);
        assert!((raw_to_voltage(4095, 12, 3.3) - 3.3).abs() < 1e-3);
        // Mid-scale ~= half vref.
        assert!((raw_to_voltage(2048, 12, 3.3) - 1.65).abs() < 1e-2);
    }

    #[test]
    fn convert_block_interleaved() {
        // Two channels, one time point: ch0=0, ch1=4095 (12-bit).
        let mut raw = Vec::new();
        raw.extend_from_slice(&0u16.to_le_bytes());
        raw.extend_from_slice(&4095u16.to_le_bytes());
        let mut out = vec![0u8; 8];
        let n = convert_block(&raw, &mut out, 12, 3.3);
        assert_eq!(n, 2);
        let v0 = f32::from_le_bytes(out[0..4].try_into().unwrap());
        let v1 = f32::from_le_bytes(out[4..8].try_into().unwrap());
        assert!((v0 - 0.0).abs() < 1e-6);
        assert!((v1 - 3.3).abs() < 1e-3);
    }

    #[test]
    fn convert_block_respects_output_capacity() {
        let raw = vec![0u8; 16]; // 8 samples
        let mut out = vec![0u8; 8]; // room for 2 f32
        assert_eq!(convert_block(&raw, &mut out, 12, 3.3), 2);
    }

    #[test]
    fn forward_fill_copies_previous() {
        let prev = [1u8, 2, 3, 4];
        let mut dst = [0u8; 4];
        assert_eq!(forward_fill(&prev, &mut dst), 4);
        assert_eq!(dst, prev);
    }
}
