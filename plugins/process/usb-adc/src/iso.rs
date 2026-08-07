//! Asynchronous isochronous IN data path via raw libusb (`libusb1-sys`).
//!
//! Implements the **pull-gated, thread-less** source model: there is no
//! background thread. A fixed pool of ISO transfers (URBs) is kept in flight
//! and [`IsoStream::drain`] is called from the plugin's `process()`; it runs
//! `libusb_handle_events_timeout(0)` (non-blocking), which fires the completion
//! callbacks synchronously on *this* thread, then hands back the bytes they
//! accumulated.
//!
//! CRITICAL — resubmit inside the callback: the completion callback copies each
//! packet's bytes into a shared accumulation buffer AND immediately resubmits
//! the transfer. Resubmitting here (rather than later, back in `drain`) keeps
//! the URB pool maximally full: `handle_events` can fire several callbacks in
//! one call, and any transfer not yet resubmitted is out of flight, so deferring
//! resubmission lets incoming microframes land with no URB to catch them and the
//! host controller silently drops them. On macOS this deferral measured ~69% of
//! the device clock; resubmitting in-callback (matching the libusb1/Python
//! reference) recovers full capture. This stays thread-less — the callback runs
//! inside the `handle_events` call that `drain` makes.
//!
//! NOTE on empty packets: an ISO IN endpoint is serviced every (micro)frame
//! whether or not the device has fresh data, so zero-length packets are normal
//! flow control — NOT lost samples — and are simply skipped. Genuine transfer
//! errors (a packet status other than COMPLETED) are counted and reported so
//! the plugin can flag a discontinuity. Detecting *dropped samples* precisely
//! is impossible in this byte-stream mode (no per-sample sequencing); that is
//! deferred to the device-clock-domain upgrade described in the design notes.
//!
//! SAFETY: this module is full of raw libusb FFI and requires real hardware to
//! exercise. The pure conversion/forward-fill logic lives in `convert.rs`.

use std::os::raw::{c_int, c_uint, c_void};

use libc::timeval;
use libusb1_sys as ffi;

const LIBUSB_TRANSFER_TYPE_ISOCHRONOUS: u8 = 1;
const LIBUSB_TRANSFER_COMPLETED: c_int = 0;

/// Shared state pointed to by every transfer's `user_data`. The completion
/// callback runs synchronously inside `handle_events` on the same thread as
/// `drain`, so plain fields behind a raw pointer are sound (no locking).
struct Shared {
    /// Bytes the callback copied out of completed packets, taken by `drain`.
    accum: Vec<u8>,
    /// Errored packets (status != COMPLETED) since the last `drain` (reset there).
    errors: u64,
    /// Lifetime count of failed resubmissions (pool-shrink diagnostic).
    submit_failures: u64,
    /// While false (teardown), the callback neither copies nor resubmits.
    running: bool,
    packet_size: usize,
    packets_per_transfer: usize,
}

extern "system" fn iso_callback(transfer: *mut ffi::libusb_transfer) {
    unsafe {
        let s = (*transfer).user_data as *mut Shared;
        if s.is_null() {
            return;
        }
        let s = &mut *s;
        // During teardown the transfers are being cancelled; don't touch their
        // buffers or resubmit (that would re-arm a stream we're tearing down).
        if !s.running {
            return;
        }
        let descs = (*transfer).iso_packet_desc.as_ptr() as *const ffi::libusb_iso_packet_descriptor;
        for i in 0..s.packets_per_transfer {
            let desc = &*descs.add(i);
            if desc.status == LIBUSB_TRANSFER_COMPLETED {
                // Append only the bytes the device actually sent; a zero-length
                // packet is an idle microframe and is skipped (no synthetic fill).
                let len = desc.actual_length as usize;
                if len > 0 {
                    let pkt = (*transfer).buffer.add(i * s.packet_size);
                    s.accum
                        .extend_from_slice(std::slice::from_raw_parts(pkt, len));
                }
            } else {
                // A real transfer error (overrun/stall/etc.), distinct from an
                // idle microframe → signal a discontinuity to the caller.
                s.errors += 1;
            }
        }
        // Resubmit immediately to keep the pool full (see module docs).
        if ffi::libusb_submit_transfer(transfer) != 0 {
            s.submit_failures += 1;
        }
    }
}

pub struct IsoStream {
    ctx: *mut ffi::libusb_context,
    transfers: Vec<*mut ffi::libusb_transfer>,
    /// Backing buffers, one per transfer. Kept alive (the transfers hold raw
    /// pointers into these) and never resized after setup — hence never read.
    #[allow(dead_code)]
    buffers: Vec<Vec<u8>>,
    /// Boxed so its address is stable across moves of `IsoStream`.
    shared: Box<Shared>,
}

// SAFETY: an IsoStream is only ever created and driven on the plugin
// subprocess's single thread (libusb events, callbacks, and drain all run
// there). The `Plugin: Send` bound is for the native runtime; the process
// runtime never moves the plugin across threads. The raw pointers are owned by
// the sibling `Device` which outlives the stream.
unsafe impl Send for IsoStream {}

impl IsoStream {
    /// Allocate the transfer pool and submit all transfers. `num_transfers` ×
    /// `packets_per_transfer` packets are kept in flight; size this to cover at
    /// least ~2× the drain interval to avoid micro-frame gaps.
    ///
    /// # Safety
    /// `ctx`/`handle` must be valid for the lifetime of the stream (owned by
    /// the `Device`). The endpoint must be an ISO IN endpoint.
    pub unsafe fn start(
        ctx: *mut ffi::libusb_context,
        handle: *mut ffi::libusb_device_handle,
        endpoint: u8,
        packet_size: usize,
        packets_per_transfer: usize,
        num_transfers: usize,
    ) -> Result<Self, String> {
        let mut shared = Box::new(Shared {
            accum: Vec::new(),
            errors: 0,
            submit_failures: 0,
            running: true,
            packet_size,
            packets_per_transfer,
        });
        let shared_ptr = &mut *shared as *mut Shared as *mut c_void;

        let mut transfers = Vec::with_capacity(num_transfers);
        let mut buffers = Vec::with_capacity(num_transfers);

        for _ in 0..num_transfers {
            let t = ffi::libusb_alloc_transfer(packets_per_transfer as c_int);
            if t.is_null() {
                return Err("libusb_alloc_transfer returned null".to_string());
            }
            let mut buf = vec![0u8; packet_size * packets_per_transfer];

            (*t).dev_handle = handle;
            (*t).endpoint = endpoint;
            (*t).transfer_type = LIBUSB_TRANSFER_TYPE_ISOCHRONOUS;
            (*t).timeout = 0;
            (*t).buffer = buf.as_mut_ptr();
            (*t).length = (packet_size * packets_per_transfer) as c_int;
            (*t).num_iso_packets = packets_per_transfer as c_int;
            (*t).callback = iso_callback;
            (*t).user_data = shared_ptr;

            // Set each ISO packet's requested length (no inline helper in -sys).
            let descs = (*t).iso_packet_desc.as_ptr() as *mut ffi::libusb_iso_packet_descriptor;
            for i in 0..packets_per_transfer {
                (*descs.add(i)).length = packet_size as c_uint;
            }

            let rc = ffi::libusb_submit_transfer(t);
            if rc != 0 {
                ffi::libusb_free_transfer(t);
                return Err(format!("libusb_submit_transfer failed: {rc}"));
            }
            transfers.push(t);
            buffers.push(buf);
        }

        Ok(IsoStream {
            ctx,
            transfers,
            buffers,
            shared,
        })
    }

    /// Number of failed transfer resubmissions observed so far (diagnostic;
    /// read by the hardware report test).
    #[allow(dead_code)]
    pub fn submit_failures(&self) -> u64 {
        self.shared.submit_failures
    }

    /// Pump libusb events (firing the completion callbacks, which copy bytes and
    /// resubmit in place), then hand back everything accumulated since the last
    /// call. Returns the number of errored packets (status != COMPLETED) seen
    /// since the last drain — a real discontinuity signal, distinct from idle
    /// (zero-length) microframes which are silently skipped.
    pub fn drain(&mut self, raw_out: &mut Vec<u8>) -> usize {
        let tv = timeval { tv_sec: 0, tv_usec: 0 };
        unsafe {
            ffi::libusb_handle_events_timeout(self.ctx, &tv);
        }
        raw_out.extend_from_slice(&self.shared.accum);
        self.shared.accum.clear();
        let errors = self.shared.errors as usize;
        self.shared.errors = 0;
        errors
    }
}

impl Drop for IsoStream {
    fn drop(&mut self) {
        // Stop the callback from resubmitting (or touching buffers) as the
        // cancellations complete.
        self.shared.running = false;
        unsafe {
            for &t in &self.transfers {
                ffi::libusb_cancel_transfer(t);
            }
            // Let cancellations complete so libusb stops touching the buffers.
            let tv = timeval {
                tv_sec: 0,
                tv_usec: 50_000,
            };
            ffi::libusb_handle_events_timeout(self.ctx, &tv);
            for &t in &self.transfers {
                ffi::libusb_free_transfer(t);
            }
        }
        self.transfers.clear();
    }
}
