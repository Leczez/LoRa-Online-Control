// sx127x/src/spi.rs

use embedded_hal::{
    delay::DelayNs,
    digital::OutputPin,
    spi::SpiDevice,
};

use crate::{Config, LoraRadio, ReceivedPacket, Sx127xError};

const REG_FIFO: u8 = 0x00;
const REG_OP_MODE: u8 = 0x01;
const REG_FRF_MSB: u8 = 0x06;
const REG_PA_CONFIG: u8 = 0x09;
const REG_FIFO_ADDR_PTR: u8 = 0x0D;
const REG_FIFO_TX_BASE_ADDR: u8 = 0x0E;
const REG_FIFO_RX_BASE_ADDR: u8 = 0x0F;
const REG_FIFO_RX_CURRENT_ADDR: u8 = 0x10;
const REG_IRQ_FLAGS: u8 = 0x12;
const REG_RX_NB_BYTES: u8 = 0x13;
const REG_PKT_RSSI_VALUE: u8 = 0x1A;
const REG_MODEM_CONFIG_1: u8 = 0x1D;
const REG_MODEM_CONFIG_2: u8 = 0x1E;
const REG_PREAMBLE_MSB: u8 = 0x20;
const REG_PAYLOAD_LENGTH: u8 = 0x22;
const REG_MODEM_CONFIG_3: u8 = 0x26;
const REG_DETECTION_OPTIMIZE: u8 = 0x31;
const REG_DETECTION_THRESHOLD: u8 = 0x37;
const REG_SYNC_WORD: u8 = 0x39;
const REG_PA_DAC: u8 = 0x4D;
#[cfg(test)]
const REG_VERSION: u8 = 0x42;

const LONG_RANGE_MODE: u8 = 0x80;
const MODE_SLEEP: u8 = 0x00;
const MODE_STDBY: u8 = 0x01;
const MODE_TX: u8 = 0x03;
const MODE_RXCONTINUOUS: u8 = 0x05;
const MODE_MASK: u8 = 0x07;

const IRQ_TX_DONE: u8 = 0x08;
const IRQ_RX_DONE: u8 = 0x40;
const IRQ_PAYLOAD_CRC_ERROR: u8 = 0x20;

const TX_POLL_ITERATIONS: u32 = 100_000;

pub struct Sx127xSpi<SPI, RESET, DELAY> {
    pub(crate) spi: SPI,
    pub(crate) reset: RESET,
    pub(crate) delay: DELAY,
    addr: u16,
}

impl<SPI, RESET, DELAY> Sx127xSpi<SPI, RESET, DELAY>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
{
    pub fn new(spi: SPI, reset: RESET, delay: DELAY) -> Self {
        Self { spi, reset, delay, addr: 0 }
    }

    fn read_register(&mut self, addr: u8) -> Result<u8, Sx127xError<SPI::Error>> {
        let mut buf = [addr & 0x7F, 0x00];
        self.spi.transfer_in_place(&mut buf).map_err(Sx127xError::Transport)?;
        Ok(buf[1])
    }

    fn write_register(&mut self, addr: u8, value: u8) -> Result<(), Sx127xError<SPI::Error>> {
        self.spi.write(&[addr | 0x80, value]).map_err(Sx127xError::Transport)
    }

    fn write_fifo(&mut self, data: &[u8]) -> Result<(), Sx127xError<SPI::Error>> {
        let mut buf = heapless::Vec::<u8, 243>::new();
        buf.push(REG_FIFO | 0x80).ok();
        buf.extend_from_slice(data).ok();
        self.spi.write(&buf).map_err(Sx127xError::Transport)
    }

    fn read_fifo(&mut self, out: &mut [u8]) -> Result<(), Sx127xError<SPI::Error>> {
        // Single transfer_in_place, matching read_register's proven-working
        // pattern, rather than a Write+Read `transaction()` — some SpiDevice
        // implementations don't guarantee CS stays continuously asserted
        // across separate operations in a transaction, which would make the
        // device see the read phase's first clocked byte as a fresh address
        // instead of FIFO data, shifting every byte read by one.
        let mut buf = heapless::Vec::<u8, 241>::new();
        buf.push(REG_FIFO & 0x7F).ok();
        buf.resize(1 + out.len(), 0).ok();
        self.spi.transfer_in_place(&mut buf).map_err(Sx127xError::Transport)?;
        out.copy_from_slice(&buf[1..]);
        Ok(())
    }

    fn hardware_reset(&mut self) -> Result<(), Sx127xError<SPI::Error>> {
        self.reset.set_low().map_err(|_| Sx127xError::InvalidConfig)?;
        self.delay.delay_ms(1);
        self.reset.set_high().map_err(|_| Sx127xError::InvalidConfig)?;
        self.delay.delay_ms(10);
        Ok(())
    }

    fn set_mode(&mut self, mode: u8) -> Result<(), Sx127xError<SPI::Error>> {
        self.write_register(REG_OP_MODE, LONG_RANGE_MODE | mode)
    }
}

impl<SPI, RESET, DELAY> LoraRadio for Sx127xSpi<SPI, RESET, DELAY>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
{
    type Error = Sx127xError<SPI::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        self.addr = config.addr;
        self.hardware_reset()?;

        // LongRangeMode can only be changed in Sleep mode.
        self.set_mode(MODE_SLEEP)?;
        self.set_mode(MODE_STDBY)?;

        let frf = config.frf_register();
        self.write_register(REG_FRF_MSB, (frf >> 16) as u8)?;
        self.write_register(REG_FRF_MSB + 1, (frf >> 8) as u8)?;
        self.write_register(REG_FRF_MSB + 2, frf as u8)?;

        let (pa_config, pa_dac) = config.pa_config_bytes();
        self.write_register(REG_PA_CONFIG, pa_config)?;
        self.write_register(REG_PA_DAC, pa_dac)?;

        let sf = config.spreading_factor.clamp(7, 12);
        self.write_register(
            REG_MODEM_CONFIG_1,
            (config.bandwidth.register_value() << 4) | (config.coding_rate.register_value() << 1),
        )?;
        self.write_register(
            REG_MODEM_CONFIG_2,
            (sf << 4) | if config.crc_on { 0x04 } else { 0x00 },
        )?;
        self.write_register(
            REG_MODEM_CONFIG_3,
            if config.low_data_rate_optimize() { 0x08 } else { 0x00 },
        )?;

        self.write_register(REG_PREAMBLE_MSB, (config.preamble_len >> 8) as u8)?;
        self.write_register(REG_PREAMBLE_MSB + 1, config.preamble_len as u8)?;
        self.write_register(REG_SYNC_WORD, config.sync_word)?;

        let (detect_optimize, detect_threshold) = config.detection_registers();
        self.write_register(REG_DETECTION_OPTIMIZE, detect_optimize)?;
        self.write_register(REG_DETECTION_THRESHOLD, detect_threshold)?;

        self.write_register(REG_FIFO_TX_BASE_ADDR, 0x00)?;
        self.write_register(REG_FIFO_RX_BASE_ADDR, 0x00)?;

        self.set_mode(MODE_STDBY)?;
        Ok(())
    }

    fn send(&mut self, _dest: u16, payload: &[u8]) -> Result<(), Self::Error> {
        self.set_mode(MODE_STDBY)?;
        self.write_register(REG_FIFO_ADDR_PTR, 0x00)?;

        let mut buf = heapless::Vec::<u8, 242>::new();
        buf.push((self.addr >> 8) as u8).ok();
        buf.push((self.addr & 0xFF) as u8).ok();
        buf.extend_from_slice(payload).ok();

        self.write_register(REG_PAYLOAD_LENGTH, buf.len() as u8)?;
        self.write_fifo(&buf)?;

        self.write_register(REG_IRQ_FLAGS, 0xFF)?;
        self.set_mode(MODE_TX)?;

        for _ in 0..TX_POLL_ITERATIONS {
            let irq = self.read_register(REG_IRQ_FLAGS)?;
            if irq & IRQ_TX_DONE != 0 {
                self.write_register(REG_IRQ_FLAGS, 0xFF)?;
                return Ok(());
            }
        }
        Err(Sx127xError::Timeout)
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error> {
        let op_mode = self.read_register(REG_OP_MODE)?;
        if op_mode & MODE_MASK != MODE_RXCONTINUOUS {
            self.write_register(REG_FIFO_RX_BASE_ADDR, 0x00)?;
            self.set_mode(MODE_RXCONTINUOUS)?;
            return Ok(None);
        }

        let irq = self.read_register(REG_IRQ_FLAGS)?;
        if irq & IRQ_RX_DONE == 0 {
            return Ok(None);
        }
        self.write_register(REG_IRQ_FLAGS, 0xFF)?;

        if irq & IRQ_PAYLOAD_CRC_ERROR != 0 {
            return Ok(None);
        }

        let len = self.read_register(REG_RX_NB_BYTES)? as usize;
        let cur_addr = self.read_register(REG_FIFO_RX_CURRENT_ADDR)?;
        self.write_register(REG_FIFO_ADDR_PTR, cur_addr)?;

        let mut raw = heapless::Vec::<u8, 240>::new();
        raw.resize(len.min(240), 0).ok();
        self.read_fifo(raw.as_mut_slice())?;

        if raw.len() < 2 {
            return Ok(None);
        }
        let src_addr = ((raw[0] as u16) << 8) | raw[1] as u16;

        let rssi_raw = self.read_register(REG_PKT_RSSI_VALUE)? as i16;
        let rssi = -164 + rssi_raw;

        let mut payload = heapless::Vec::<u8, 240>::new();
        payload.extend_from_slice(&raw[2..]).ok();

        Ok(Some(ReceivedPacket { src_addr, rssi: Some(rssi), payload }))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use embedded_hal_mock::eh1::{
        delay::NoopDelay,
        pin::{Mock as PinMock, State, Transaction as PinTx},
        spi::{Mock as SpiMock, Transaction as SpiTx},
    };

    #[test]
    fn test_read_register_returns_version() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_VERSION & 0x7F, 0x00], std::vec![0x00, 0x12]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        assert_eq!(radio.read_register(REG_VERSION).unwrap(), 0x12);

        radio.spi.done();
        radio.reset.done();
    }

    #[test]
    fn test_read_fifo_single_transfer_no_byte_shift() {
        // Regression test: read_fifo must issue one continuous transfer
        // (address byte + N dummy bytes) so the device sees a single
        // uninterrupted read, not two separate transactions that would make
        // it reinterpret the read phase's first byte as a fresh address.
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(
                std::vec![REG_FIFO & 0x7F, 0x00, 0x00, 0x00],
                std::vec![0x00, 0xAA, 0xBB, 0xCC],
            ),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        let mut out = [0u8; 3];
        radio.read_fifo(&mut out).unwrap();
        assert_eq!(out, [0xAA, 0xBB, 0xCC]);

        radio.spi.done();
        radio.reset.done();
    }

    #[test]
    fn test_write_register_sets_write_bit() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_SYNC_WORD | 0x80, 0x34]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        radio.write_register(REG_SYNC_WORD, 0x34).unwrap();

        radio.spi.done();
        radio.reset.done();
    }

    #[test]
    fn test_hardware_reset_toggles_pin() {
        let spi = SpiMock::<u8>::new(&[]);
        let reset = PinMock::new(&[PinTx::set(State::Low), PinTx::set(State::High)]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        radio.hardware_reset().unwrap();

        radio.spi.done();
        radio.reset.done();
    }
}
