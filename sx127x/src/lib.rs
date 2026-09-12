#![no_std]

#[cfg(feature = "std")]
extern crate std;

pub mod config;
pub use config::{Bandwidth, CodingRate, Config};

pub mod spi;
pub use spi::Sx127xSpi;

use heapless::Vec;

/// Placeholder DIO0 pin for `Sx127xSpi` instances that don't wire up a
/// hardware interrupt line — completion (TX/CAD done) is detected by
/// polling IRQ_FLAGS over SPI instead, exactly as before this type existed.
/// Its `InputPin` impl is never actually exercised (the driver only reads
/// `dio0` when a real pin was provided via `new_with_dio0`); it exists
/// purely so `Sx127xSpi`'s default type parameter has something concrete
/// to be.
#[derive(Debug, Default)]
pub struct NoInputPin;

impl embedded_hal::digital::ErrorType for NoInputPin {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::InputPin for NoInputPin {
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(false)
    }
    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A DIO0 pin (or equivalent) capable of a genuine blocking wait for its
/// own rising edge — typically backed by a real hardware GPIO interrupt —
/// rather than the plain level-polling `new_with_dio0`'s `InputPin` bound
/// gets you. Entirely optional and additive: `new_with_dio0`'s existing
/// polling behavior is unchanged and doesn't require this trait at all;
/// this is only for `new_with_dio0_waiter`, for a platform that can do
/// better than polling.
///
/// The implementor owns the *entire* wait, timeout included — unlike the
/// `InputPin` polling path (where `wait_for` itself drives the poll loop
/// and the delay between checks), `wait_for` calls `wait_high` exactly
/// once per wait and trusts it to honor `timeout_us` internally (e.g. via
/// a real interrupt plus a task notification with a timeout, letting the
/// CPU actually idle instead of spinning). See esp32-node's own DIO0
/// wrapper for a concrete ESP-IDF-backed implementation.
pub trait DioWait {
    type Error: core::fmt::Debug;
    /// Blocks until this pin's rising edge occurs, or `timeout_us`
    /// microseconds elapse — whichever comes first. `Ok(true)` on a real
    /// edge, `Ok(false)` on timeout.
    fn wait_high(&mut self, timeout_us: u32) -> Result<bool, Self::Error>;
}

/// Placeholder for `Sx127xSpi` instances constructed without a real
/// `DioWait` implementation (i.e. anything using `new`/`new_with_dio0`
/// rather than `new_with_dio0_waiter`) — mirrors `NoInputPin`'s role for
/// the polling path. Never actually exercised.
#[derive(Debug, Default)]
pub struct NoWaiter;

impl DioWait for NoWaiter {
    type Error = core::convert::Infallible;
    fn wait_high(&mut self, _timeout_us: u32) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

/// A packet received from the radio.
#[derive(Debug)]
pub struct ReceivedPacket {
    pub src_addr: u16,
    /// Signal strength in dBm. None if RSSI reporting was disabled in Config.
    pub rssi: Option<i16>,
    /// Raw payload bytes. Max 240 bytes (largest SX127x FIFO budget after
    /// the 2-byte address prefix).
    pub payload: Vec<u8, 240>,
}

/// Common interface for sx127x transport implementations.
pub trait LoraRadio {
    type Error;

    /// Apply configuration to the module. Must be called before send/receive.
    fn configure(&mut self, config: &Config) -> Result<(), Self::Error>;

    /// Transmit payload to dest_addr.
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error>;

    /// Non-blocking receive poll. Returns Ok(None) if no message is available.
    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error>;
}

/// Which operation's completion was never observed — see
/// `Sx127xError::Timeout`. `send()` waits on two of these in sequence
/// (`ChannelActivityDetection` first, then `TxDone`), and without this the
/// two failure modes were indistinguishable from the outside: a CAD that
/// never completes means nothing was even keyed up for transmission yet, a
/// TxDone that never arrives means the transmission itself started but its
/// completion was never signaled — different enough to matter when
/// diagnosing a field failure from logs alone, with no way to attach a
/// debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// Channel Activity Detection (send()'s pre-TX collision check) never
    /// completed.
    ChannelActivityDetection,
    /// The chip never signaled TX completion within the poll budget.
    TxDone,
}

/// Error type for sx127x drivers. E is the underlying transport error.
#[derive(Debug)]
pub enum Sx127xError<E> {
    /// Underlying SPI hardware error.
    Transport(E),
    /// Parameter value not supported by the hardware.
    InvalidConfig,
    /// Module did not respond within the poll budget — see `TimeoutKind`
    /// for which operation this was waiting on.
    Timeout(TimeoutKind),
    /// `send()`'s payload won't fit in the FIFO alongside the 2-byte address
    /// prefix. Rejected up front rather than silently dropped, so a caller
    /// never mistakes "nothing was actually transmitted" for success.
    PayloadTooLarge { len: usize, max: usize },
    /// Channel Activity Detection found another LoRa transmission already
    /// in progress — `send()` declined to transmit rather than collide with
    /// it. Not itself retried inside the driver; callers already have their
    /// own retry-on-failure logic (this looks like any other failed send).
    ChannelBusy,
}

#[cfg(feature = "std")]
impl<E: core::fmt::Debug> std::error::Error for Sx127xError<E> {}

impl<E: core::fmt::Debug> core::fmt::Display for Sx127xError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Sx127xError::Transport(e) => write!(f, "transport error: {:?}", e),
            Sx127xError::InvalidConfig => write!(f, "invalid configuration"),
            Sx127xError::Timeout(TimeoutKind::ChannelActivityDetection) => {
                write!(f, "module did not respond: channel activity detection never completed")
            }
            Sx127xError::Timeout(TimeoutKind::TxDone) => {
                write!(f, "module did not respond: TX completion (TxDone) never observed")
            }
            Sx127xError::PayloadTooLarge { len, max } => {
                write!(f, "payload too large: {} bytes (max {})", len, max)
            }
            Sx127xError::ChannelBusy => write!(f, "channel busy (CAD detected activity)"),
        }
    }
}
