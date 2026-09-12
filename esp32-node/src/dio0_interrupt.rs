//! A real, interrupt-backed `sx127x::DioWait` implementation for this
//! board's DIO0 pin (GPIO7 — see the wiring doc) — see `DioWait`'s own doc
//! comment in the `sx127x` crate for why this exists: a genuine blocking
//! wait, letting the CPU actually idle between checks, instead of
//! `new_with_dio0`'s plain-`InputPin` polling. On by default (`dio0-
//! interrupt` feature) — see Cargo.toml's own doc comment on that feature
//! for why, and how to build the plain-polling variant instead if this
//! needs to be ruled in or out during field debugging.
//!
//! Built on esp-idf-hal's GPIO interrupt support
//! (`PinDriver::set_interrupt_type`/`subscribe`/`enable_interrupt`) and
//! esp-idf-svc's `Notification` — the standard, ISR-safe way in this
//! ecosystem to wake a blocked task from an interrupt callback without
//! doing anything an ISR context can't safely do (no allocation, no
//! blocking calls, no logging inside the callback itself).
//!
//! **UNVERIFIED**: this file could not be compiled in the sandbox that
//! wrote it (no xtensa/esp-idf toolchain available there at all). The
//! exact esp-idf-hal 0.45 / esp-idf-svc 0.51 method names and signatures
//! used below (`subscribe`, `enable_interrupt`, `Notification::wait`,
//! `TickType::new_millis`) are written from well-documented, standard
//! patterns for GPIO-interrupt-plus-notification code in this ecosystem,
//! but need a real `cargo build --release --features dio0-interrupt` to
//! confirm before trusting this around a real interrupt — more so than any
//! other change in this codebase tonight. If it doesn't compile, the
//! method names above are the first thing to check against whatever
//! exact esp-idf-hal/esp-idf-svc version actually resolves.

use core::num::NonZeroU32;

use esp_idf_hal::delay::TickType;
use esp_idf_hal::gpio::{Gpio7, Input, InterruptType, PinDriver};
use esp_idf_svc::hal::task::notification::Notification;

pub struct Dio0Interrupt<'d> {
    pin: PinDriver<'d, Gpio7, Input>,
    notification: Notification,
}

impl<'d> Dio0Interrupt<'d> {
    /// Takes ownership of an already-created input `PinDriver` for the
    /// physical DIO0 pin and arms it for a rising-edge interrupt (DIO0 is
    /// asserted high by the SX127x to signal completion — see
    /// `sx127x`'s `DIO0_MAPPING_TXDONE`/`DIO0_MAPPING_CADDONE`). The
    /// interrupt itself is left disabled until each `wait_high` call
    /// re-enables it — matching `new_with_dio0`'s existing pattern of only
    /// touching DIO0 when actually waiting on it, and required anyway
    /// since ESP-IDF auto-masks a GPIO interrupt once it fires.
    pub fn new(mut pin: PinDriver<'d, Gpio7, Input>) -> anyhow::Result<Self> {
        pin.set_interrupt_type(InterruptType::PosEdge)?;
        let notification = Notification::new();
        let notifier = notification.notifier();
        // Safety: the callback only touches `notifier` — a small handle
        // designed specifically for signaling from ISR context — and does
        // nothing else: no allocation, no blocking, no logging. This is
        // the one thing ESP-IDF's interrupt context actually allows.
        unsafe {
            pin.subscribe(move || {
                notifier.notify_and_yield(NonZeroU32::new(1).unwrap());
            })?;
        }
        Ok(Self { pin, notification })
    }
}

impl<'d> sx127x::DioWait for Dio0Interrupt<'d> {
    type Error = anyhow::Error;

    fn wait_high(&mut self, timeout_us: u32) -> Result<bool, Self::Error> {
        // Re-armed on every call, not left permanently enabled — see
        // `new`'s doc comment on why.
        self.pin.enable_interrupt()?;
        // Ceiling-divided (not truncated) so a short configured timeout
        // (e.g. a few milliseconds of CAD budget at low SF) never rounds
        // down to less real wait time than was actually requested — the
        // FreeRTOS tick this ultimately becomes can't represent
        // microsecond precision anyway, so this only ever rounds up to the
        // next whole millisecond, never down.
        let timeout_ms = timeout_us.div_ceil(1000).max(1) as u64;
        let notified = self.notification.wait(TickType::new_millis(timeout_ms).ticks());
        Ok(notified.is_some())
    }
}
