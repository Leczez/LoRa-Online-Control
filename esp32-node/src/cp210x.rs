//! Thin safe(ish) wrapper around ESP-IDF's USB Host + CDC-ACM + CP210x VCP
//! C API (bound via `cp210x_bindings.h` / `esp_idf_sys::cp210x::*`), used to
//! read the SportIdent master station.
//!
//! Unlike a plain UART, CDC-ACM's receive side is callback-based, not a
//! blocking read: the driver's own internal task calls a registered C
//! callback whenever bytes arrive. `Cp210xTransport` bridges that into the
//! `std::io::Read`/`Write` shape `sportident::SiReader<T>` expects by
//! funneling received chunks through an `mpsc` channel into an internal
//! staging buffer that `read()` drains non-blockingly (`Ok(0)` when nothing's
//! arrived yet, same as `sportident.rs` already treats a timeout).
//!
//! Hotplug: the same `user_arg` also carries an `Arc<AtomicBool>` set by a
//! registered `event_cb` on `CDC_ACM_HOST_DEVICE_DISCONNECTED`. Read/Write
//! check it before touching the (now-dead) USB handle and fail fast with
//! `ErrorKind::NotConnected`; `main.rs` checks `is_disconnected()` each loop
//! iteration and, on disconnect, drops this transport (closing the handle in
//! `Drop`) and calls `wait_for_si_master` again — mirroring the RPi side's
//! udev-hotplug reconnect loop in `lora-server/src/sportident.rs`, just
//! driven by this driver's own event callback instead of udev.
//!
//! **This is the one part of esp32-node I have no way to runtime-test in a
//! sandbox** — no CP210x hardware to simulate USB transfers (or unplug
//! events) against. It compiles against the real ESP-IDF v5.3.1 headers and
//! the actual `usb_host_cp210x_vcp` component fetched via the component
//! registry, but needs real hardware to confirm the open/read/write/
//! disconnect sequence actually works end to end.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::sys::cp210x::{
    cdc_acm_data_callback_t, cdc_acm_dev_hdl_t, cdc_acm_host_data_tx_blocking,
    cdc_acm_host_dev_callback_t, cdc_acm_host_dev_event_data_t,
    cdc_acm_host_dev_event_t_CDC_ACM_HOST_DEVICE_DISCONNECTED, cdc_acm_host_device_config_t,
    cdc_acm_host_driver_config_t, cdc_acm_host_install, cp210x_vcp_open,
};
use esp_idf_svc::sys::{esp, usb_host_config_t, usb_host_install, usb_host_lib_handle_events, EspError};

/// Installs the USB Host Library and the CDC-ACM host driver, and spawns the
/// background task that services USB Host Library events. Must be called
/// exactly once before `Cp210xTransport::open`.
pub fn install() -> anyhow::Result<()> {
    let host_config = usb_host_config_t {
        skip_phy_setup: false,
        intr_flags: 0,
        enum_filter_cb: None,
    };
    esp!(unsafe { usb_host_install(&host_config) })?;

    // ESP-IDF's documented pattern: after usb_host_install(), something must
    // keep calling usb_host_lib_handle_events() for the library to actually
    // process transfers/enumeration — it does nothing on its own otherwise.
    std::thread::Builder::new()
        .name("usb-host-lib".into())
        .stack_size(4096)
        .spawn(|| loop {
            let mut event_flags: u32 = 0;
            unsafe {
                usb_host_lib_handle_events(u32::MAX, &mut event_flags);
            }
        })?;

    let driver_config = cdc_acm_host_driver_config_t {
        driver_task_stack_size: 4096,
        driver_task_priority: 5,
        xCoreID: 0,
        new_dev_cb: None,
    };
    esp!(unsafe { cdc_acm_host_install(&driver_config) })?;

    Ok(())
}

/// Bundled behind one `user_arg` pointer since `cdc_acm_host_device_config_t`
/// only has a single `void *user_arg`, shared by both `data_cb` and
/// `event_cb`.
struct CallbackState {
    tx: Sender<Vec<u8>>,
    disconnected: Arc<AtomicBool>,
}

extern "C" fn data_callback(data: *const u8, data_len: usize, user_arg: *mut c_void) -> bool {
    // Safety: `user_arg` points at the `CallbackState` boxed in `open()`,
    // kept alive for the device's lifetime via `Cp210xTransport::_state`.
    let state = unsafe { &*(user_arg as *const CallbackState) };
    // Safety: `data`/`data_len` describe a valid buffer for the duration of
    // this callback, per cdc_acm_data_callback_t's documented contract.
    let bytes = unsafe { std::slice::from_raw_parts(data, data_len) }.to_vec();
    let _ = state.tx.send(bytes);
    true // data consumed, driver may reuse its RX buffer
}

extern "C" fn event_callback(event: *const cdc_acm_host_dev_event_data_t, user_arg: *mut c_void) {
    // Safety: same `CallbackState` as data_callback above.
    let state = unsafe { &*(user_arg as *const CallbackState) };
    // Safety: `event` is valid for the duration of this callback.
    let event = unsafe { &*event };
    if event.type_ == cdc_acm_host_dev_event_t_CDC_ACM_HOST_DEVICE_DISCONNECTED {
        state.disconnected.store(true, Ordering::SeqCst);
    }
}

pub struct Cp210xTransport {
    hdl: cdc_acm_dev_hdl_t,
    rx: Receiver<Vec<u8>>,
    staging: VecDeque<u8>,
    disconnected: Arc<AtomicBool>,
    // Kept alive for the device's lifetime — both callbacks dereference this
    // via the raw pointer handed to cp210x_vcp_open as user_arg.
    _state: Box<CallbackState>,
}

impl Cp210xTransport {
    /// Opens the SI master (VID 0x10C4, the given `pid`) and configures the
    /// line for `baud` 8N1 — matching `sportident::SI_PID`/`SI_BAUD`.
    pub fn open(pid: u16, baud: u32) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let disconnected = Arc::new(AtomicBool::new(false));
        let state = Box::new(CallbackState { tx, disconnected: Arc::clone(&disconnected) });
        let user_arg = Box::as_ref(&state) as *const CallbackState as *mut c_void;

        let data_cb: cdc_acm_data_callback_t = Some(data_callback);
        let event_cb: cdc_acm_host_dev_callback_t = Some(event_callback);
        let dev_config = cdc_acm_host_device_config_t {
            connection_timeout_ms: 5000,
            out_buffer_size: 256,
            in_buffer_size: 256,
            event_cb,
            data_cb,
            user_arg,
        };

        let mut hdl: cdc_acm_dev_hdl_t = std::ptr::null_mut();
        esp!(unsafe { cp210x_vcp_open(pid, 0, &dev_config, &mut hdl) })?;

        let mut transport = Self { hdl, rx, staging: VecDeque::new(), disconnected, _state: state };
        transport.set_baud(baud)?;
        Ok(transport)
    }

    /// True once the driver has reported this device disconnected. Checked
    /// by `main.rs`'s loop to trigger dropping this transport and
    /// reconnecting via `wait_for_si_master` again.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    fn set_baud(&mut self, baud: u32) -> Result<(), EspError> {
        use esp_idf_svc::sys::cp210x::{cdc_acm_host_line_coding_set, cdc_acm_line_coding_t};
        let line_coding = cdc_acm_line_coding_t {
            dwDTERate: baud,
            bCharFormat: 0, // 1 stop bit
            bParityType: 0, // none
            bDataBits: 8,
        };
        esp!(unsafe { cdc_acm_host_line_coding_set(self.hdl, &line_coding) })
    }

    fn drain_channel(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.staging.extend(chunk);
        }
    }
}

impl io::Read for Cp210xTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.is_disconnected() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "SI master disconnected"));
        }
        self.drain_channel();
        let n = self.staging.len().min(buf.len());
        for (i, byte) in self.staging.drain(..n).enumerate() {
            buf[i] = byte;
        }
        Ok(n)
    }
}

impl io::Write for Cp210xTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.is_disconnected() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "SI master disconnected"));
        }
        esp!(unsafe { cdc_acm_host_data_tx_blocking(self.hdl, buf.as_ptr(), buf.len(), 1000) })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Cp210xTransport {
    fn drop(&mut self) {
        // Safe to call even after a physical disconnect — cdc_acm_host_close
        // just releases the driver's own bookkeeping for a handle it already
        // knows is gone. Deliberately not called from event_callback itself
        // (documented as running in constrained USB Host context); this runs
        // later on whatever thread drops the transport instead.
        unsafe {
            esp_idf_svc::sys::cp210x::cdc_acm_host_close(self.hdl);
        }
    }
}

/// Blocks until the SI master enumerates (retries `open` every 2s), since
/// unlike the RPi side there's no udev hotplug signal to wait on here. Used
/// both for the initial connection and to reconnect after a disconnect.
pub fn wait_for_si_master(pid: u16, baud: u32) -> Cp210xTransport {
    loop {
        match Cp210xTransport::open(pid, baud) {
            Ok(t) => return t,
            Err(e) => {
                log::warn!("SI master not found yet ({}), retrying...", e);
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}
