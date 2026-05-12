#![no_std]

pub mod config;
pub use config::{AirSpeed, BufferSize, Config, TxPower};

use heapless::Vec;

/// A packet received from the radio.
#[derive(Debug)]
pub struct ReceivedPacket {
    pub src_addr: u16,
    /// Signal strength in dBm. None if RSSI reporting was disabled in Config.
    pub rssi: Option<i16>,
    /// Raw payload bytes. Max 240 bytes (largest SX126x buffer setting).
    pub payload: Vec<u8, 240>,
}

/// Common interface for all radio transport implementations.
pub trait LoraRadio {
    type Error;

    /// Apply configuration to the module. Must be called before send/receive.
    fn configure(&mut self, config: &Config) -> Result<(), Self::Error>;

    /// Transmit payload to dest_addr.
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error>;

    /// Non-blocking receive poll. Returns Ok(None) if no message is available.
    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error>;
}

/// Error type for sx126x drivers. E is the underlying transport error.
#[derive(Debug)]
pub enum Sx126xError<E> {
    /// Underlying serial/SPI hardware error.
    Transport(E),
    /// Parameter value not supported by the hardware.
    InvalidConfig,
    /// Module did not respond during configuration.
    Timeout,
}

#[cfg(feature = "std")]
impl<E: core::fmt::Debug> std::error::Error for Sx126xError<E> {}

impl<E: core::fmt::Debug> core::fmt::Display for Sx126xError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Sx126xError::Transport(e) => write!(f, "transport error: {:?}", e),
            Sx126xError::InvalidConfig => write!(f, "invalid configuration"),
            Sx126xError::Timeout => write!(f, "module did not respond"),
        }
    }
}

/// A no-op GPIO pin for platforms without GPIO (e.g., plain Linux desktop via USB serial).
/// Implements OutputPin by doing nothing.
pub struct NoPin;

impl embedded_hal::digital::ErrorType for NoPin {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::OutputPin for NoPin {
    fn set_low(&mut self) -> Result<(), Self::Error> { Ok(()) }
    fn set_high(&mut self) -> Result<(), Self::Error> { Ok(()) }
}
