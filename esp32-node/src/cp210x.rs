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
//! **This is the one part of esp32-node I have no way to runtime-test in a
//! sandbox** — no CP210x hardware to simulate USB transfers against. It
//! compiles against the real ESP-IDF v5.3.1 headers and the actual
//! `usb_host_cp210x_vcp` component fetched via the component registry, but
//! needs real hardware to confirm the open/read/write sequence actually
//! works end to end.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use esp_idf_svc::sys::cp210x::{
    cdc_acm_data_callback_t, cdc_acm_dev_hdl_t, cdc_acm_host_data_tx_blocking,
    cdc_acm_host_device_config_t, cdc_acm_host_driver_config_t, cdc_acm_host_install,
    cp210x_vcp_open,
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

extern "C" fn data_callback(data: *const u8, data_len: usize, user_arg: *mut c_void) -> bool {
    // Safety: `user_arg` was created from `Box::into_raw(Box<Sender<Vec<u8>>>)`
    // in `open()` and lives for the device's lifetime (never freed while the
    // device is open, matching how this driver expects a stable user_arg).
    let tx = unsafe { &*(user_arg as *const Sender<Vec<u8>>) };
    // Safety: `data`/`data_len` describe a valid buffer for the duration of
    // this callback, per cdc_acm_data_callback_t's documented contract.
    let bytes = unsafe { std::slice::from_raw_parts(data, data_len) }.to_vec();
    let _ = tx.send(bytes);
    true // data consumed, driver may reuse its RX buffer
}

pub struct Cp210xTransport {
    hdl: cdc_acm_dev_hdl_t,
    rx: Receiver<Vec<u8>>,
    staging: VecDeque<u8>,
    // Kept alive for the device's lifetime — data_callback dereferences this
    // via the raw pointer handed to cp210x_vcp_open as user_arg.
    _tx_box: Box<Sender<Vec<u8>>>,
}

impl Cp210xTransport {
    /// Opens the SI master (VID 0x10C4, the given `pid`) and configures the
    /// line for `baud` 8N1 — matching `sportident::SI_PID`/`SI_BAUD`.
    pub fn open(pid: u16, baud: u32) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let tx_box = Box::new(tx);
        let user_arg = Box::as_ref(&tx_box) as *const Sender<Vec<u8>> as *mut c_void;

        let data_cb: cdc_acm_data_callback_t = Some(data_callback);
        let dev_config = cdc_acm_host_device_config_t {
            connection_timeout_ms: 5000,
            out_buffer_size: 256,
            in_buffer_size: 256,
            event_cb: None,
            data_cb,
            user_arg,
        };

        let mut hdl: cdc_acm_dev_hdl_t = std::ptr::null_mut();
        esp!(unsafe { cp210x_vcp_open(pid, 0, &dev_config, &mut hdl) })?;

        let mut transport = Self { hdl, rx, staging: VecDeque::new(), _tx_box: tx_box };
        transport.set_baud(baud)?;
        Ok(transport)
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
        unsafe {
            esp_idf_svc::sys::cp210x::cdc_acm_host_close(self.hdl);
        }
    }
}

/// Blocks until the SI master enumerates (retries `open` every 2s), since
/// unlike the RPi side there's no udev hotplug signal to wait on here.
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
