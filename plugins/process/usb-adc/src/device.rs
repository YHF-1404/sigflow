//! Device control plane over rusb (safe libusb wrapper).
//!
//! Handles open / reset / interface claim and the four vendor control
//! requests (GET_CAPABILITY, SET_ADC_CONFIG, PIPE_START, PIPE_STOP). The
//! isochronous data path (`iso.rs`) reuses this handle's raw libusb pointers.
//!
//! All methods require a connected device; they are exercised on real hardware
//! (the pure protocol encode/decode lives in `protocol.rs` and is unit-tested).

use std::time::Duration;

use rusb::{Context, DeviceHandle, UsbContext};

use sigflow_plugin_sdk::mono_ns;

use crate::protocol::{
    AdcConfig, Capability, DeviceTimeObs, Transport, TriggerEvents, BM_IN, BM_OUT, CAP_SIZE,
    DEV_TIME_SIZE, EP_IN, PID_BULK, PID_ISO, REQ_GET_CAPABILITY, REQ_GET_DEVICE_TIME,
    REQ_GET_TRIGGER_EVENTS, REQ_PIPE_START, REQ_PIPE_STOP, REQ_SET_ADC_CONFIG, TRIG_EVENTS_SIZE,
    VID,
};

const CTRL_TIMEOUT: Duration = Duration::from_millis(2000);

/// An opened USB ADC device.
pub struct Device {
    // `context` must outlive `handle`; both expose raw pointers the iso layer
    // uses. Field order matters for drop (handle before context).
    handle: DeviceHandle<Context>,
    context: Context,
    /// Which firmware variant enumerated (bulk PID preferred over ISO).
    transport: Transport,
}

impl Device {
    /// Open the device, optionally forcing a USB port reset to clear stale
    /// DWC2 ISO endpoint state, then claim interface 0.
    pub fn open(reset_on_open: bool) -> Result<Self, String> {
        let context = Context::new().map_err(|e| format!("libusb context: {e}"))?;

        // Prefer the bulk-transport firmware (Phase 2), fall back to ISO —
        // both speak the same vendor-request set, only the data path differs.
        let (mut handle, transport) = match context.open_device_with_vid_pid(VID, PID_BULK) {
            Some(h) => (h, Transport::Bulk),
            None => match context.open_device_with_vid_pid(VID, PID_ISO) {
                Some(h) => (h, Transport::Iso),
                None => {
                    return Err(format!(
                        "device VID={VID:#06x} PID={PID_BULK:#06x}(bulk)/{PID_ISO:#06x}(iso) \
                         not found (check USB cable, firmware, and udev rules on Linux)"
                    ))
                }
            },
        };
        let pid = match transport {
            Transport::Bulk => PID_BULK,
            Transport::Iso => PID_ISO,
        };

        // Best-effort: let libusb detach a kernel driver on claim (Linux only).
        let _ = handle.set_auto_detach_kernel_driver(true);

        // Port reset exists to clear the DWC2 iso_parity residue — an ISO-only
        // quirk. The bulk firmware has no such state, and field evidence ties
        // port resets on an actively-streaming bulk device to device-side
        // lockups ~2-3 s later (blackbox: tick frozen, no fault; dmesg: both
        // crashes 2-3 s after a `reset high-speed USB device`, no
        // over-current). Plugin restarts then re-reset the device, amplifying
        // into a reboot loop. Skip the reset entirely on bulk.
        if reset_on_open && transport == Transport::Iso {
            // A port reset re-enumerates the device; libusb may invalidate the
            // handle (NotFound), so re-open afterwards. Mirrors the Python ref.
            if handle.reset().is_err() {
                handle = context
                    .open_device_with_vid_pid(VID, pid)
                    .ok_or_else(|| "device vanished after reset".to_string())?;
                let _ = handle.set_auto_detach_kernel_driver(true);
            }
        }

        handle
            .claim_interface(0)
            .map_err(|e| format!("claim interface 0: {e}"))?;

        eprintln!("usb_adc: opened {transport:?} device (PID {pid:#06x})");
        Ok(Device { handle, context, transport })
    }

    /// Which data transport the connected firmware speaks.
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// Query the device capability structure.
    pub fn get_capability(&self) -> Result<Capability, String> {
        let mut buf = [0u8; CAP_SIZE];
        let n = self
            .handle
            .read_control(BM_IN, REQ_GET_CAPABILITY, 0, 0, &mut buf, CTRL_TIMEOUT)
            .map_err(|e| format!("GET_CAPABILITY: {e}"))?;
        Capability::parse(&buf[..n])
    }

    /// Read the device's free-running sample counter, bracketed by the host
    /// monotonic clock, as one clock-recovery observation. The control round
    /// trip bounds the offset accuracy (`rtt_ns`); the rate is recovered over
    /// many observations regardless.
    pub fn get_device_time(&self) -> Result<DeviceTimeObs, String> {
        let mut buf = [0u8; DEV_TIME_SIZE];
        let t_send = mono_ns();
        let n = self
            .handle
            .read_control(BM_IN, REQ_GET_DEVICE_TIME, 0, 0, &mut buf, CTRL_TIMEOUT)
            .map_err(|e| format!("GET_DEVICE_TIME: {e}"))?;
        let t_recv = mono_ns();
        if n < DEV_TIME_SIZE {
            return Err(format!("GET_DEVICE_TIME short read: {n} < {DEV_TIME_SIZE}"));
        }
        Ok(DeviceTimeObs {
            sample_count: u64::from_le_bytes(buf),
            host_mono_mid_ns: t_send + (t_recv - t_send) / 2,
            rtt_ns: t_recv - t_send,
        })
    }

    /// Drain the device's pen-trigger event FIFO. Each event carries the device
    /// sample index latched at the trigger edge (acquisition clock domain) and
    /// which pen fired. Cheap when empty; poll periodically.
    pub fn get_trigger_events(&self) -> Result<TriggerEvents, String> {
        let mut buf = [0u8; TRIG_EVENTS_SIZE];
        let n = self
            .handle
            .read_control(BM_IN, REQ_GET_TRIGGER_EVENTS, 0, 0, &mut buf, CTRL_TIMEOUT)
            .map_err(|e| format!("GET_TRIGGER_EVENTS: {e}"))?;
        TriggerEvents::parse(&buf[..n])
    }

    /// Send the ADC configuration to the firmware.
    pub fn set_adc_config(&self, cfg: &AdcConfig) -> Result<(), String> {
        let payload = cfg.encode();
        self.handle
            .write_control(BM_OUT, REQ_SET_ADC_CONFIG, 0, 0, &payload, CTRL_TIMEOUT)
            .map(|_| ())
            .map_err(|e| format!("SET_ADC_CONFIG: {e}"))
    }

    pub fn pipe_start(&self) -> Result<(), String> {
        self.handle
            .write_control(BM_OUT, REQ_PIPE_START, 0, 0, &[], CTRL_TIMEOUT)
            .map(|_| ())
            .map_err(|e| format!("PIPE_START: {e}"))
    }

    pub fn pipe_stop(&self) -> Result<(), String> {
        self.handle
            .write_control(BM_OUT, REQ_PIPE_STOP, 0, 0, &[], CTRL_TIMEOUT)
            .map(|_| ())
            .map_err(|e| format!("PIPE_STOP: {e}"))
    }

    /// The IN endpoint the ISO data streams from.
    pub fn ep_in(&self) -> u8 {
        EP_IN
    }

    /// Raw libusb handle pointer (for the async ISO layer).
    pub fn raw_handle(&self) -> *mut libusb1_sys::libusb_device_handle {
        self.handle.as_raw()
    }

    /// Raw libusb context pointer (for `handle_events`).
    pub fn raw_context(&self) -> *mut libusb1_sys::libusb_context {
        self.context.as_raw()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Best-effort stop + release; ignore errors during teardown.
        let _ = self.pipe_stop();
        let _ = self.handle.release_interface(0);
    }
}
