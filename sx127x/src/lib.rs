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
