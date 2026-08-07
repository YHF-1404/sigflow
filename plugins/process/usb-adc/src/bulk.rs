//! Asynchronous bulk IN data path via raw libusb (`libusb1-sys`).
//!
//! Same pull-gated, thread-less architecture as `iso.rs`: a fixed pool of
//! bulk transfers stays in flight, [`BulkStream::drain`] pumps
//! `libusb_handle_events_timeout(0)` from the plugin's `process()`, and the
//! completion callback copies bytes out and resubmits in place.
//!
//! Why async instead of synchronous reads with timeouts: canceling a bulk IN
//! URB against an actively delivering endpoint is fundamentally lossy on
//! xHCI/usbfs — packets ACKed on the wire during the Stop-Endpoint window
//! land in the dying URB and are discarded beyond the reported `transferred`.
//! Field forensics (per-block seq logs on both ends) showed blocks vanishing
//! exactly at every timeout boundary, at any timeout value, because the
//! stream's cadence is irregular (device ramp-up, flow-control modulation).
//! With a standing pool nothing is ever canceled while streaming; transfers
//! are canceled only in `Drop`, after the pipe is quiescent.
//!
//! The firmware sends fixed 512-byte max-packet blocks (never a short
//! packet), so every transfer completes only when FULL: `actual_length` is
//! always a whole number of blocks and the byte stream stays 512-aligned.
//! A half-filled transfer simply stays pending across stream pauses — at
//! most one transfer's worth of data is held back, never lost.

use std::os::raw::{c_int, c_void};

use libc::timeval;
use libusb1_sys as ffi;

const LIBUSB_TRANSFER_TYPE_BULK: u8 = 2;
const LIBUSB_TRANSFER_STATUS_COMPLETED: c_int = 0;

/// Shared state pointed to by every transfer's `user_data`. The completion
/// callback runs synchronously inside `handle_events` on the same thread as
/// `drain`, so plain fields behind a raw pointer are sound (no locking).
struct Shared {
    /// Bytes copied out of completed transfers, taken by `drain`. Bulk
    /// transfers on one endpoint complete in submission order, so this stays
    /// in stream order.
    accum: Vec<u8>,
    /// Transfers that completed with a non-COMPLETED status since the last
    /// drain. Complete 512 B blocks inside them are salvaged into `accum`
    /// (dropping them would manufacture a hole); only a torn tail is cut.
    errors: u64,
    /// Last non-COMPLETED transfer status (diagnostic).
    last_error_status: c_int,
    /// Bytes salvaged from errored transfers since the last drain.
    salvaged: u64,
    /// Block alignment for salvage truncation.
    block_align: usize,
    /// Lifetime count of failed resubmissions (pool-shrink diagnostic).
    submit_failures: u64,
    /// While false (teardown), the callback neither copies nor resubmits.
    running: bool,
}

extern "system" fn bulk_callback(transfer: *mut ffi::libusb_transfer) {
    unsafe {
        let s = (*transfer).user_data as *mut Shared;
        if s.is_null() {
            return;
        }
        let s = &mut *s;
        if !s.running {
            return;
        }
        let len = (*transfer).actual_length as usize;
        if (*transfer).status == LIBUSB_TRANSFER_STATUS_COMPLETED {
            if len > 0 {
                s.accum
                    .extend_from_slice(std::slice::from_raw_parts((*transfer).buffer, len));
            }
        } else {
            // Salvage the complete blocks an errored transfer already
            // received — discarding them would turn a transient status into
            // a data hole. Cut only a torn (non-block-aligned) tail.
            s.errors += 1;
            s.last_error_status = (*transfer).status;
            let keep = len - (len % s.block_align);
            if keep > 0 {
                s.accum
                    .extend_from_slice(std::slice::from_raw_parts((*transfer).buffer, keep));
                s.salvaged += keep as u64;
            }
        }
        // Resubmit immediately to keep the pool full (queue order preserved:
        // this transfer rejoins at the tail).
        if ffi::libusb_submit_transfer(transfer) != 0 {
            s.submit_failures += 1;
        }
    }
}

pub struct BulkStream {
    ctx: *mut ffi::libusb_context,
    transfers: Vec<*mut ffi::libusb_transfer>,
    /// Backing buffers, one per transfer; alive for the transfers' lifetime.
    #[allow(dead_code)]
    buffers: Vec<Vec<u8>>,
    /// Boxed so its address is stable across moves of `BulkStream`.
    shared: Box<Shared>,
}

// SAFETY: only ever created and driven on the plugin subprocess's single
// thread (same argument as `IsoStream`).
unsafe impl Send for BulkStream {}

impl BulkStream {
    /// Allocate the transfer pool and submit all transfers.
    ///
    /// `transfer_bytes` must be a multiple of the device's fixed block size
    /// (512) so transfers only complete when full and the stream stays
    /// block-aligned. `num_transfers × transfer_bytes` is the in-flight
    /// buffering window.
    ///
    /// # Safety
    /// `ctx`/`handle` must be valid for the lifetime of the stream (owned by
    /// the `Device`). The endpoint must be a bulk IN endpoint.
    pub unsafe fn start(
        ctx: *mut ffi::libusb_context,
        handle: *mut ffi::libusb_device_handle,
        endpoint: u8,
        transfer_bytes: usize,
        num_transfers: usize,
        block_align: usize,
    ) -> Result<Self, String> {
        let mut shared = Box::new(Shared {
            accum: Vec::new(),
            errors: 0,
            last_error_status: 0,
            salvaged: 0,
            block_align: block_align.max(1),
            submit_failures: 0,
            running: true,
        });
        let shared_ptr = &mut *shared as *mut Shared as *mut c_void;

        let mut transfers = Vec::with_capacity(num_transfers);
        let mut buffers = Vec::with_capacity(num_transfers);

        for _ in 0..num_transfers {
            let t = ffi::libusb_alloc_transfer(0);
            if t.is_null() {
                return Err("libusb_alloc_transfer returned null".to_string());
            }
            let mut buf = vec![0u8; transfer_bytes];

            (*t).dev_handle = handle;
            (*t).endpoint = endpoint;
            (*t).transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
            (*t).timeout = 0; // never times out → never canceled while streaming
            (*t).buffer = buf.as_mut_ptr();
            (*t).length = transfer_bytes as c_int;
            (*t).num_iso_packets = 0;
            (*t).callback = bulk_callback;
            (*t).user_data = shared_ptr;

            let rc = ffi::libusb_submit_transfer(t);
            if rc != 0 {
                ffi::libusb_free_transfer(t);
                return Err(format!("libusb_submit_transfer failed: {rc}"));
            }
            transfers.push(t);
            buffers.push(buf);
        }

        Ok(BulkStream {
            ctx,
            transfers,
            buffers,
            shared,
        })
    }

    /// Lifetime count of failed in-callback resubmissions (each one shrinks
    /// the standing pool by one transfer — a structural health signal).
    pub fn submit_failures(&self) -> u64 {
        self.shared.submit_failures
    }

    /// Pump libusb events (firing completions, which copy bytes and resubmit
    /// in place), then hand back everything accumulated since the last call.
    /// Returns `(errors, last_error_status, salvaged_bytes)` since the last
    /// drain (all zero in the healthy case).
    pub fn drain(&mut self, raw_out: &mut Vec<u8>) -> (usize, i32, u64) {
        let tv = timeval { tv_sec: 0, tv_usec: 0 };
        unsafe {
            ffi::libusb_handle_events_timeout(self.ctx, &tv);
        }
        raw_out.extend_from_slice(&self.shared.accum);
        self.shared.accum.clear();
        let errors = self.shared.errors as usize;
        let status = self.shared.last_error_status;
        let salvaged = self.shared.salvaged;
        self.shared.errors = 0;
        self.shared.last_error_status = 0;
        self.shared.salvaged = 0;
        (errors, status, salvaged)
    }
}

impl Drop for BulkStream {
    fn drop(&mut self) {
        self.shared.running = false;
        unsafe {
            for &t in &self.transfers {
                ffi::libusb_cancel_transfer(t);
            }
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
