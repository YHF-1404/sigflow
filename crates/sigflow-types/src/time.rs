//! Shared monotonic clock source.
//!
//! Every node, source, and plugin stamps time with the *same* `CLOCK_MONOTONIC`
//! reading so timestamps are directly comparable across processes on one host
//! (Linux: `CLOCK_MONOTONIC` is system-wide; macOS: `clock_gettime(CLOCK_MONOTONIC)`
//! reads a system-wide mach time base). This is the basis for self-anchored
//! *absolute* `t0_ns` (so cross-source alignment has a common zero) and for
//! latency measurement (`now − t0_ns` = how old the data is at this point).
//!
//! Only differences and same-host cross-process comparisons are meaningful; the
//! absolute value (time since boot) carries no wall-clock meaning. A separate
//! (wall, mono) anchor pair is the future upgrade for human-readable / cross-host
//! time.

/// Current `CLOCK_MONOTONIC` reading in nanoseconds.
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
