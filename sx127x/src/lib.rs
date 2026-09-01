#![no_std]

#[cfg(feature = "std")]
extern crate std;

pub mod config;
pub use config::{Bandwidth, CodingRate, Config};

pub mod spi;
pub use spi::Sx127xSpi;

pub use sx126x::ReceivedPacket;

/// Common interface for sx127x transport implementations. Shaped like
/// `sx126x::LoraRadio`, but bound to this crate's own `Config` type.
pub trait LoraRadio {
    type Error;

    /// Apply configuration to the module. Must be called before send/receive.
    fn configure(&mut self, config: &Config) -> Result<(), Self::Error>;

    /// Transmit payload to dest_addr.
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error>;

    /// Non-blocking receive poll. Returns Ok(None) if no message is available.
    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error>;
}

/// Error type for sx127x drivers. E is the underlying transport error.
#[derive(Debug)]
pub enum Sx127xError<E> {
    /// Underlying SPI hardware error.
    Transport(E),
    /// Parameter value not supported by the hardware.
    InvalidConfig,
    /// Module did not respond (e.g. TxDone/RxDone never set) within the poll budget.
    Timeout,
}

#[cfg(feature = "std")]
impl<E: core::fmt::Debug> std::error::Error for Sx127xError<E> {}

impl<E: core::fmt::Debug> core::fmt::Display for Sx127xError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Sx127xError::Transport(e) => write!(f, "transport error: {:?}", e),
            Sx127xError::InvalidConfig => write!(f, "invalid configuration"),
            Sx127xError::Timeout => write!(f, "module did not respond"),
        }
    }
}
