// sx127x/src/spi.rs

use embedded_hal::{
    delay::DelayNs,
    digital::{InputPin, OutputPin},
    spi::SpiDevice,
};

use crate::{Config, DioWait, LoraRadio, NoInputPin, NoWaiter, ReceivedPacket, Sx127xError, TimeoutKind};

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
const REG_DIO_MAPPING1: u8 = 0x40;

const LONG_RANGE_MODE: u8 = 0x80;
const MODE_SLEEP: u8 = 0x00;
const MODE_STDBY: u8 = 0x01;
const MODE_TX: u8 = 0x03;
const MODE_RXCONTINUOUS: u8 = 0x05;
const MODE_CAD: u8 = 0x07;
const MODE_MASK: u8 = 0x07;

const IRQ_TX_DONE: u8 = 0x08;
const IRQ_RX_DONE: u8 = 0x40;
const IRQ_PAYLOAD_CRC_ERROR: u8 = 0x20;
const IRQ_CAD_DONE: u8 = 0x04;
const IRQ_CAD_DETECTED: u8 = 0x01;

// Dio0Mapping (RegDioMapping1 bits 7:6) — which IRQ_FLAGS event DIO0
// reflects; only one at a time, so `wait_for` repoints it before each wait.
// receive()'s own poll stays SPI-based rather than DIO0 deliberately: it's
// meant to keep observing RxDone continuously while idle, which one shared
// pin can't do at the same time it's repointed at TxDone/CadDone for a
// send() or CAD check in between — no RXDONE mapping constant is needed
// since nothing here ever points DIO0 at it.
const DIO0_MAPPING_TXDONE: u8 = 0x40;
const DIO0_MAPPING_CADDONE: u8 = 0x80;

/// Which blocking wait `wait_for` is doing, and how to observe it either
/// way: the IRQ_FLAGS bit to poll over SPI, or how DIO0 should be mapped to
/// reflect the same event directly when a DIO0 pin is wired.
enum WaitFor {
    TxDone,
    CadDone,
}

impl WaitFor {
    fn irq_bit(&self) -> u8 {
        match self {
            WaitFor::TxDone => IRQ_TX_DONE,
            WaitFor::CadDone => IRQ_CAD_DONE,
        }
    }
    fn dio0_mapping(&self) -> u8 {
        match self {
            WaitFor::TxDone => DIO0_MAPPING_TXDONE,
            WaitFor::CadDone => DIO0_MAPPING_CADDONE,
        }
    }
    fn timeout_kind(&self) -> TimeoutKind {
        match self {
            WaitFor::TxDone => TimeoutKind::TxDone,
            WaitFor::CadDone => TimeoutKind::ChannelActivityDetection,
        }
    }
}

pub struct Sx127xSpi<SPI, RESET, DELAY, DIO0 = NoInputPin, W = NoWaiter> {
    pub(crate) spi: SPI,
    pub(crate) reset: RESET,
    pub(crate) delay: DELAY,
    dio0: Option<DIO0>,
    /// Set only by `new_with_dio0_waiter` — a real blocking-with-timeout
    /// wait (see `DioWait`), checked by `wait_for` before falling back to
    /// `dio0`'s plain polling. Mutually exclusive with `dio0` in practice
    /// (a given instance uses one DIO0 strategy or the other), but nothing
    /// stops both being `Some` at the type level — `wait_for` just always
    /// prefers `waiter` when present.
    waiter: Option<W>,
    addr: u16,
    /// The last `Config` applied via `configure()` — kept around so
    /// `wait_for`'s CAD/TX timeouts can be computed from the *actual*
    /// SF/BW/CR in use (see `Config::cad_timeout_us`/`tx_timeout_us`)
    /// rather than a single fixed value that can only ever be right for
    /// one setting. `Default::default()` here is never actually used for a
    /// real wait — `configure()` must succeed before `send()`/CAD are ever
    /// called — it just avoids needing an `Option` for a field every
    /// method already assumes is populated.
    config: Config,
}

impl<SPI, RESET, DELAY> Sx127xSpi<SPI, RESET, DELAY, NoInputPin, NoWaiter>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
{
    /// Completion (TX/CAD) is detected by polling IRQ_FLAGS over SPI — the
    /// only option without a DIO0 pin wired. See `new_with_dio0`/
    /// `new_with_dio0_waiter` for lower-overhead alternatives.
    pub fn new(spi: SPI, reset: RESET, delay: DELAY) -> Self {
        Self { spi, reset, delay, dio0: None, waiter: None, addr: 0, config: Config::default() }
    }
}

impl<SPI, RESET, DELAY, DIO0> Sx127xSpi<SPI, RESET, DELAY, DIO0, NoWaiter>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
    DIO0: InputPin,
{
    /// Like `new`, but completion (TX/CAD) is detected by polling the
    /// module's DIO0 pin directly — a plain GPIO read, far cheaper than an
    /// SPI transaction, so this busy-waits much more efficiently than the
    /// SPI-polling default. Requires wiring the module's DIO0 pin to `dio0`;
    /// its mapping (which event it reflects) is reconfigured automatically
    /// for whichever operation is about to run. See `new_with_dio0_waiter`
    /// for a real-interrupt alternative on a platform that supports one.
    pub fn new_with_dio0(spi: SPI, reset: RESET, delay: DELAY, dio0: DIO0) -> Self {
        Self { spi, reset, delay, dio0: Some(dio0), waiter: None, addr: 0, config: Config::default() }
    }
}

impl<SPI, RESET, DELAY, W> Sx127xSpi<SPI, RESET, DELAY, NoInputPin, W>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
    W: DioWait,
{
    /// Like `new_with_dio0`, but completion (TX/CAD) is detected via a
    /// genuine blocking wait (typically backed by a real hardware
    /// interrupt) rather than polling — see `DioWait`'s doc comment. `wait`
    /// owns its own timeout entirely; `wait_for` calls it exactly once per
    /// wait rather than driving a poll loop itself. See esp32-node's own
    /// DIO0 wrapper for a concrete ESP-IDF-backed implementation.
    pub fn new_with_dio0_waiter(spi: SPI, reset: RESET, delay: DELAY, wait: W) -> Self {
        Self { spi, reset, delay, dio0: None, waiter: Some(wait), addr: 0, config: Config::default() }
    }
}

impl<SPI, RESET, DELAY, DIO0, W> Sx127xSpi<SPI, RESET, DELAY, DIO0, W>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
    DIO0: InputPin,
    W: DioWait,
{
    /// Only used by the `dio0` (plain polling) path — `waiter`'s own
    /// `wait_high` owns its entire timeout internally, no re-checking
    /// needed here. Re-checks completion every `POLL_INTERVAL_US` — far
    /// shorter than any real CAD/TX duration (milliseconds or more at any
    /// supported SF/BW), so this adds nothing meaningful to how quickly
    /// completion is actually detected. Using a real delay between checks,
    /// rather than a bare unthrottled spin, is what lets `timeout_us` mean
    /// actual elapsed time instead of an iteration count that only ever
    /// corresponded to some particular amount of wall-clock time for one
    /// specific CPU and one specific SF/BW (see `Config::cad_timeout_us`/
    /// `tx_timeout_us`, which compute the right bound for whichever
    /// SF/BW/CR is actually configured).
    const POLL_INTERVAL_US: u32 = 100;

    fn wait_for(&mut self, event: WaitFor, timeout_us: u32) -> Result<u8, Sx127xError<SPI::Error>> {
        if self.waiter.is_some() {
            self.write_register(REG_DIO_MAPPING1, event.dio0_mapping())?;
            // Safe: confirmed Some above, and this borrow doesn't overlap
            // the self.read_register(..) call below it.
            let high = self.waiter.as_mut().unwrap().wait_high(timeout_us).map_err(|_| Sx127xError::InvalidConfig)?;
            return if high {
                self.read_register(REG_IRQ_FLAGS)
            } else {
                Err(Sx127xError::Timeout(event.timeout_kind()))
            };
        }

        let max_polls = (timeout_us / Self::POLL_INTERVAL_US).max(1);
        if self.dio0.is_some() {
            self.write_register(REG_DIO_MAPPING1, event.dio0_mapping())?;
            for _ in 0..max_polls {
                // Safe: confirmed Some above, and this borrow doesn't
                // overlap the self.read_register(..) call below it.
                let high = self.dio0.as_mut().unwrap().is_high().map_err(|_| Sx127xError::InvalidConfig)?;
                if high {
                    return self.read_register(REG_IRQ_FLAGS);
                }
                self.delay.delay_us(Self::POLL_INTERVAL_US);
            }
            return Err(Sx127xError::Timeout(event.timeout_kind()));
        }

        for _ in 0..max_polls {
            let irq = self.read_register(REG_IRQ_FLAGS)?;
            if irq & event.irq_bit() != 0 {
                return Ok(irq);
            }
            self.delay.delay_us(Self::POLL_INTERVAL_US);
        }
        Err(Sx127xError::Timeout(event.timeout_kind()))
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

    /// Channel Activity Detection: scans for an in-progress LoRa preamble on
    /// the configured frequency/SF. LoRa itself has no listen-before-talk —
    /// this is the chip's own optional support for building one, used by
    /// `send()` as a basic collision check (not a lock/reservation, just a
    /// "does it look busy right now" read before keying up TX).
    fn channel_activity_detected(&mut self) -> Result<bool, Sx127xError<SPI::Error>> {
        self.write_register(REG_IRQ_FLAGS, 0xFF)?;
        self.set_mode(MODE_CAD)?;
        let irq = self.wait_for(WaitFor::CadDone, self.config.cad_timeout_us())?;
        self.write_register(REG_IRQ_FLAGS, 0xFF)?;
        Ok(irq & IRQ_CAD_DETECTED != 0)
    }
}

impl<SPI, RESET, DELAY, DIO0, W> LoraRadio for Sx127xSpi<SPI, RESET, DELAY, DIO0, W>
where
    SPI: SpiDevice,
    RESET: OutputPin,
    DELAY: DelayNs,
    DIO0: InputPin,
    W: DioWait,
{
    type Error = Sx127xError<SPI::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        self.addr = config.addr;
        // Cached so wait_for's CAD/TX timeouts (Config::cad_timeout_us/
        // tx_timeout_us) can be computed from whatever SF/BW/CR is actually
        // in effect, not a single value that could only ever be right for
        // one setting.
        self.config = config.clone();
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
        // Checked up front, before touching the radio at all: heapless::Vec's
        // extend_from_slice is all-or-nothing, so silently letting it fail
        // here would mean transmitting just the 2-byte address prefix with
        // no indication anything was dropped — the caller would see Ok(())
        // for a transmission that carried none of its actual payload.
        const MAX_PAYLOAD: usize = 240;
        if payload.len() > MAX_PAYLOAD {
            return Err(Sx127xError::PayloadTooLarge { len: payload.len(), max: MAX_PAYLOAD });
        }

        self.set_mode(MODE_STDBY)?;

        // LoRa has no built-in collision avoidance — CAD is the closest
        // thing the chip offers: a scan for an in-progress transmission on
        // this frequency/SF before we key up. Not a guarantee (a signal
        // could start between this check and our own TX), just a basic
        // "don't blindly transmit into an obviously busy channel."
        if self.channel_activity_detected()? {
            self.set_mode(MODE_STDBY)?;
            return Err(Sx127xError::ChannelBusy);
        }

        self.write_register(REG_FIFO_ADDR_PTR, 0x00)?;

        // Capacity is guaranteed by the length check above (2-byte prefix +
        // up to MAX_PAYLOAD fits the 242-byte buffer with room to spare) —
        // expect() turns "silently truncated" into "loud bug report" if
        // that invariant is ever violated by a future change here.
        let mut buf = heapless::Vec::<u8, 242>::new();
        buf.push((self.addr >> 8) as u8).expect("address prefix always fits");
        buf.push((self.addr & 0xFF) as u8).expect("address prefix always fits");
        buf.extend_from_slice(payload).expect("payload length already validated");

        self.write_register(REG_PAYLOAD_LENGTH, buf.len() as u8)?;
        self.write_fifo(&buf)?;

        self.write_register(REG_IRQ_FLAGS, 0xFF)?;
        self.set_mode(MODE_TX)?;
        // buf.len() (not payload.len()) — the actual over-the-air byte
        // count, including the 2-byte address prefix, is what determines
        // real airtime.
        self.wait_for(WaitFor::TxDone, self.config.tx_timeout_us(buf.len()))?;
        self.write_register(REG_IRQ_FLAGS, 0xFF)?;
        Ok(())
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
    fn test_send_happy_path_clear_channel_transmits_successfully() {
        let spi = SpiMock::<u8>::new(&[
            // send(): set_mode(STDBY)
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_STDBY]),
            SpiTx::transaction_end(),
            // channel_activity_detected(): clear IRQ flags
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            // channel_activity_detected(): set_mode(CAD)
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_CAD]),
            SpiTx::transaction_end(),
            // channel_activity_detected(): poll — CadDone set, CadDetected clear (channel free)
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, IRQ_CAD_DONE]),
            SpiTx::transaction_end(),
            // channel_activity_detected(): clear IRQ flags after CAD
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            // send(): write_register(FIFO_ADDR_PTR, 0)
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_FIFO_ADDR_PTR | 0x80, 0x00]),
            SpiTx::transaction_end(),
            // send(): write_register(PAYLOAD_LENGTH, 3) — 2 addr bytes + 1 payload byte
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_PAYLOAD_LENGTH | 0x80, 0x03]),
            SpiTx::transaction_end(),
            // send(): write_fifo([addr_hi=0, addr_lo=0, 0xAB])
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_FIFO | 0x80, 0x00, 0x00, 0xAB]),
            SpiTx::transaction_end(),
            // send(): write_register(IRQ_FLAGS, 0xFF) before TX
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            // send(): set_mode(TX)
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_TX]),
            SpiTx::transaction_end(),
            // send(): poll — TxDone set
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, IRQ_TX_DONE]),
            SpiTx::transaction_end(),
            // send(): clear IRQ flags after TxDone
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        radio.send(0, &[0xAB]).unwrap();

        radio.spi.done();
        radio.reset.done();
    }

    #[test]
    fn test_send_returns_channel_busy_when_cad_detects_activity() {
        // Proves two things: send() surfaces CAD-detected activity as an
        // error instead of transmitting over it, and it stops there — no
        // FIFO write or TX mode transition happens (the mock has no more
        // expected transactions and would panic on any extra one).
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_STDBY]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_CAD]),
            SpiTx::transaction_end(),
            // poll — CadDone AND CadDetected set (channel busy)
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(
                std::vec![REG_IRQ_FLAGS & 0x7F, 0x00],
                std::vec![0x00, IRQ_CAD_DONE | IRQ_CAD_DETECTED],
            ),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            // send() returns to STDBY before reporting the error
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_STDBY]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        let err = radio.send(0, &[0xAB]).unwrap_err();
        assert!(matches!(err, Sx127xError::ChannelBusy));

        radio.spi.done();
        radio.reset.done();
    }

    #[test]
    fn test_send_via_dio0_happy_path_clear_channel_transmits_successfully() {
        // Same scenario as the SPI-polling happy path, but with a DIO0 pin
        // wired: wait_for should map DIO0 to the right event, poll the pin
        // (not IRQ_FLAGS) for readiness, and only touch SPI once more per
        // wait to read the detail bits (CadDetected) or clear flags.
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_STDBY]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_CAD]),
            SpiTx::transaction_end(),
            // wait_for(CadDone) via DIO0: map DIO0 to CadDone, then (after
            // the pin goes high, checked via the dio0 mock, not SPI) a
            // single IRQ_FLAGS read for the CadDetected detail bit.
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_DIO_MAPPING1 | 0x80, DIO0_MAPPING_CADDONE]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, IRQ_CAD_DONE]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_FIFO_ADDR_PTR | 0x80, 0x00]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_PAYLOAD_LENGTH | 0x80, 0x03]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_FIFO | 0x80, 0x00, 0x00, 0xAB]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_TX]),
            SpiTx::transaction_end(),
            // wait_for(TxDone) via DIO0: map DIO0 to TxDone, then one
            // IRQ_FLAGS read once the pin goes high.
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_DIO_MAPPING1 | 0x80, DIO0_MAPPING_TXDONE]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, IRQ_TX_DONE]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let dio0 = PinMock::new(&[PinTx::get(State::High), PinTx::get(State::High)]);
        let mut radio = Sx127xSpi::new_with_dio0(spi, reset, NoopDelay, dio0);

        radio.send(0, &[0xAB]).unwrap();

        radio.spi.done();
        radio.reset.done();
        radio.dio0.unwrap().done();
    }

    #[test]
    fn test_send_via_dio0_returns_channel_busy_when_cad_detects_activity() {
        // Mirrors the SPI-polling busy-channel test, but via DIO0: proves
        // the pin-based path also stops immediately on a detected channel
        // (no FIFO/TX transactions), and that only a single IRQ_FLAGS read
        // was needed to learn CadDetected, not a polling loop.
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_STDBY]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_CAD]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_DIO_MAPPING1 | 0x80, DIO0_MAPPING_CADDONE]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(
                std::vec![REG_IRQ_FLAGS & 0x7F, 0x00],
                std::vec![0x00, IRQ_CAD_DONE | IRQ_CAD_DETECTED],
            ),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_IRQ_FLAGS | 0x80, 0xFF]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_OP_MODE | 0x80, LONG_RANGE_MODE | MODE_STDBY]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let dio0 = PinMock::new(&[PinTx::get(State::High)]);
        let mut radio = Sx127xSpi::new_with_dio0(spi, reset, NoopDelay, dio0);

        let err = radio.send(0, &[0xAB]).unwrap_err();
        assert!(matches!(err, Sx127xError::ChannelBusy));

        radio.spi.done();
        radio.reset.done();
        radio.dio0.unwrap().done();
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
    fn test_send_rejects_oversized_payload_without_touching_the_radio() {
        // Regression test for the silent-drop bug: extend_from_slice on the
        // internal buffer is all-or-nothing, so an oversized payload used to
        // vanish entirely while send() still reported Ok(()). It must now
        // fail loudly, and before any SPI transaction is even issued.
        let spi = SpiMock::<u8>::new(&[]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        let oversized = std::vec![0u8; 241];
        let err = radio.send(0, &oversized).unwrap_err();
        match err {
            Sx127xError::PayloadTooLarge { len, max } => {
                assert_eq!(len, 241);
                assert_eq!(max, 240);
            }
            other => panic!("expected PayloadTooLarge, got {:?}", other),
        }

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

    /// Regression guard for the CAD/TxDone timeout distinction: without it,
    /// a CAD that never completes (nothing was even keyed up for
    /// transmission yet) was indistinguishable from a TxDone that never
    /// arrives (the transmission started but its completion was never
    /// signaled) — both surfaced as the exact same generic error. Passes a
    /// small `timeout_us` (200, i.e. exactly `2 * POLL_INTERVAL_US`) rather
    /// than a real CAD/TX timeout, so this exercises exactly 2 polls before
    /// giving up — quick and deterministic with `NoopDelay`, which makes
    /// every `delay_us` call an instant no-op regardless of the value
    /// passed.
    #[test]
    fn test_wait_for_cad_timeout_reports_channel_activity_detection_kind() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, 0x00]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, 0x00]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let mut radio = Sx127xSpi::new(spi, reset, NoopDelay);

        let result = radio.wait_for(WaitFor::CadDone, 200);
        assert!(matches!(result, Err(Sx127xError::Timeout(TimeoutKind::ChannelActivityDetection))));

        radio.spi.done();
        radio.reset.done();
    }

    /// Same distinction, exercised via the DIO0-polling branch (not just the
    /// SPI-register-polling one above) since both construct the error from
    /// the same `event.timeout_kind()` call but are otherwise separate code
    /// paths.
    #[test]
    fn test_wait_for_via_dio0_txdone_timeout_reports_txdone_kind() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_DIO_MAPPING1 | 0x80, DIO0_MAPPING_TXDONE]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let dio0 = PinMock::new(&[PinTx::get(State::Low), PinTx::get(State::Low)]);
        let mut radio = Sx127xSpi::new_with_dio0(spi, reset, NoopDelay, dio0);

        let result = radio.wait_for(WaitFor::TxDone, 200);
        assert!(matches!(result, Err(Sx127xError::Timeout(TimeoutKind::TxDone))));

        radio.spi.done();
        radio.reset.done();
        radio.dio0.unwrap().done();
    }

    /// A `DioWait` test double — queued `Ok(bool)`/`Err(())` responses,
    /// returned in order, one per `wait_high` call. Unlike `PinMock`-backed
    /// `dio0`, this owns its entire wait (including "timing out"), matching
    /// what a real interrupt-backed implementation does — see `DioWait`'s
    /// doc comment.
    struct MockWaiter {
        responses: std::vec::Vec<Result<bool, ()>>,
    }

    impl DioWait for MockWaiter {
        type Error = ();
        fn wait_high(&mut self, _timeout_us: u32) -> Result<bool, ()> {
            assert!(!self.responses.is_empty(), "wait_high called more times than expected");
            self.responses.remove(0)
        }
    }

    /// Proves `wait_for` prefers `waiter` over `dio0`/SPI-polling when
    /// present, and — unlike the polling paths — calls `wait_high` exactly
    /// once per wait rather than looping, since the waiter owns its own
    /// timeout.
    #[test]
    fn test_wait_for_via_waiter_happy_path_calls_wait_high_exactly_once() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_DIO_MAPPING1 | 0x80, DIO0_MAPPING_CADDONE]),
            SpiTx::transaction_end(),
            SpiTx::transaction_start(),
            SpiTx::transfer_in_place(std::vec![REG_IRQ_FLAGS & 0x7F, 0x00], std::vec![0x00, IRQ_CAD_DONE]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let waiter = MockWaiter { responses: std::vec![Ok(true)] };
        let mut radio = Sx127xSpi::new_with_dio0_waiter(spi, reset, NoopDelay, waiter);

        let irq = radio.wait_for(WaitFor::CadDone, 30_000).unwrap();
        assert_eq!(irq, IRQ_CAD_DONE);
        assert!(radio.waiter.unwrap().responses.is_empty(), "wait_high should have been called exactly once");

        radio.spi.done();
        radio.reset.done();
    }

    /// A `wait_high` timeout (`Ok(false)`) is a clean, expected outcome —
    /// no retry inside `wait_for` itself, since the waiter already spent
    /// the full `timeout_us` internally.
    #[test]
    fn test_wait_for_via_waiter_timeout_reports_correct_kind() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![REG_DIO_MAPPING1 | 0x80, DIO0_MAPPING_TXDONE]),
            SpiTx::transaction_end(),
        ]);
        let reset = PinMock::new(&[]);
        let waiter = MockWaiter { responses: std::vec![Ok(false)] };
        let mut radio = Sx127xSpi::new_with_dio0_waiter(spi, reset, NoopDelay, waiter);

        let result = radio.wait_for(WaitFor::TxDone, 30_000);
        assert!(matches!(result, Err(Sx127xError::Timeout(TimeoutKind::TxDone))));

        radio.spi.done();
        radio.reset.done();
    }
}
