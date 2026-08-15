//! Shared monotonic clock source.
//!
//! Every node, source, and plugin stamps time with the *same* monotonic clock
//! reading so timestamps are directly comparable across processes on one host
//! (Linux: `CLOCK_MONOTONIC` is system-wide; macOS: `clock_gettime(CLOCK_MONOTONIC)`
//! reads a system-wide mach time base; Windows: `QueryPerformanceCounter` is
//! system-wide and monotonic). This is the basis for self-anchored *absolute*
//! `t0_ns` (so cross-source alignment has a common zero) and for latency
//! measurement (`now − t0_ns` = how old the data is at this point).
//!
//! Only differences and same-host cross-process comparisons are meaningful; the
//! absolute value (time since boot) carries no wall-clock meaning. A separate
//! (wall, mono) anchor pair is the future upgrade for human-readable / cross-host
//! time.

/// Current monotonic clock reading in nanoseconds.
#[cfg(not(windows))]
pub fn mono_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC is always
    // available on Linux and macOS. Return value is ignored — the call cannot
    // fail for a supported clock id with a valid pointer.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Current monotonic clock reading in nanoseconds.
///
/// QPC, declared directly against kernel32 (two stable-forever functions —
/// not worth a `windows-sys` dependency, matching this workspace's habit of
/// avoiding deps for one call).
#[cfg(windows)]
pub fn mono_ns() -> i64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }

    // The frequency is fixed at boot; cache it so the steady-state cost is a
    // single QPC call.
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f: i64 = 0;
        // SAFETY: valid pointer; cannot fail on XP+.
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });

    let mut count: i64 = 0;
    // SAFETY: valid pointer; cannot fail on XP+.
    unsafe { QueryPerformanceCounter(&mut count) };

    // Split-scale to nanoseconds: `count * 1e9` overflows i64 within seconds
    // of uptime at the common 10 MHz frequency, so scale the whole seconds
    // and the sub-second remainder separately.
    let secs = count / freq;
    let rem = count % freq;
    secs * 1_000_000_000 + (rem as i128 * 1_000_000_000 / freq as i128) as i64
}
