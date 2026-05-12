// sx126x/src/spi.rs

use embedded_hal::{
    digital::{InputPin, OutputPin},
    spi::SpiDevice,
};

use crate::{Config, LoraRadio, ReceivedPacket, Sx126xError};

const CMD_SET_STANDBY: u8           = 0x80;
const CMD_SET_PACKET_TYPE: u8       = 0x8A;
const CMD_SET_RF_FREQUENCY: u8      = 0x86;
const CMD_SET_TX_PARAMS: u8         = 0x8E;
const CMD_SET_MODULATION_PARAMS: u8 = 0x8B;
const CMD_SET_PACKET_PARAMS: u8     = 0x8C;
const CMD_SET_DIO_IRQ_PARAMS: u8    = 0x08;
const CMD_SET_RX: u8                = 0x82;
const CMD_SET_TX: u8                = 0x83;
const CMD_WRITE_BUFFER: u8          = 0x0E;
const CMD_READ_BUFFER: u8           = 0x1E;
const CMD_GET_IRQ_STATUS: u8        = 0x12;
const CMD_CLEAR_IRQ_STATUS: u8      = 0x02;
const CMD_GET_RX_BUFFER_STATUS: u8  = 0x13;

const PACKET_TYPE_LORA: u8 = 0x01;
const STANDBY_RC: u8       = 0x00;

pub struct Sx126xSpi<SPI, BUSY, RESET> {
    pub(crate) spi: SPI,
    pub(crate) busy: BUSY,
    pub(crate) reset: RESET,
    rssi_enabled: bool,
}

impl<SPI, BUSY, RESET> Sx126xSpi<SPI, BUSY, RESET>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
{
    pub fn new(spi: SPI, busy: BUSY, reset: RESET) -> Self {
        Self { spi, busy, reset, rssi_enabled: false }
    }

    pub fn freq_to_register(freq_mhz: u32) -> u32 {
        ((freq_mhz as u64 * 1_000_000 * (1 << 25)) / 32_000_000) as u32
    }

    fn wait_busy(&mut self) {
        for _ in 0..10_000 {
            if self.busy.is_low().unwrap_or(true) {
                break;
            }
        }
    }

    pub fn send_command(&mut self, cmd: &[u8]) -> Result<(), Sx126xError<SPI::Error>> {
        self.wait_busy();
        self.spi.write(cmd).map_err(Sx126xError::Transport)
    }

    fn write_cmd_data(&mut self, opcode: u8, data: &[u8]) -> Result<(), Sx126xError<SPI::Error>> {
        self.wait_busy();
        let mut buf = heapless::Vec::<u8, 32>::new();
        buf.push(opcode).ok();
        buf.extend_from_slice(data).ok();
        self.spi.write(&buf).map_err(Sx126xError::Transport)
    }

    fn read_cmd(&mut self, opcode: u8, out: &mut [u8]) -> Result<(), Sx126xError<SPI::Error>> {
        self.wait_busy();
        let mut cmd = [opcode, 0x00];
        self.spi.transfer_in_place(&mut cmd).map_err(Sx126xError::Transport)?;
        self.spi.read(out).map_err(Sx126xError::Transport)?;
        Ok(())
    }

    fn hardware_reset(&mut self) -> Result<(), Sx126xError<SPI::Error>> {
        self.reset.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.reset.set_high().map_err(|_| Sx126xError::InvalidConfig)?;
        Ok(())
    }
}

impl<SPI, BUSY, RESET> LoraRadio for Sx126xSpi<SPI, BUSY, RESET>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
{
    type Error = Sx126xError<SPI::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        self.rssi_enabled = config.rssi;
        self.hardware_reset()?;
        self.write_cmd_data(CMD_SET_STANDBY, &[STANDBY_RC])?;
        self.write_cmd_data(CMD_SET_PACKET_TYPE, &[PACKET_TYPE_LORA])?;

        let rf_freq = Self::freq_to_register(config.freq_mhz);
        self.write_cmd_data(CMD_SET_RF_FREQUENCY, &[
            (rf_freq >> 24) as u8,
            (rf_freq >> 16) as u8,
            (rf_freq >> 8) as u8,
            rf_freq as u8,
        ])?;

        let power_byte = match config.power {
            crate::TxPower::Dbm22 => 22i8 as u8,
            crate::TxPower::Dbm17 => 17i8 as u8,
            crate::TxPower::Dbm13 => 13i8 as u8,
            crate::TxPower::Dbm10 => 10i8 as u8,
        };
        self.write_cmd_data(CMD_SET_TX_PARAMS, &[power_byte, 0x04])?;
        self.write_cmd_data(CMD_SET_MODULATION_PARAMS, &[0x07, 0x04, 0x01, 0x00])?;
        self.write_cmd_data(CMD_SET_PACKET_PARAMS, &[0x00, 0x08, 0x00, 0xFF, 0x01, 0x00])?;
        self.write_cmd_data(CMD_SET_DIO_IRQ_PARAMS, &[
            0x00, 0x03, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
        ])?;
        Ok(())
    }

    fn send(&mut self, _dest: u16, payload: &[u8]) -> Result<(), Self::Error> {
        self.wait_busy();
        let mut cmd = heapless::Vec::<u8, 242>::new();
        cmd.push(CMD_WRITE_BUFFER).ok();
        cmd.push(0x00).ok();
        cmd.extend_from_slice(payload).ok();
        self.spi.write(&cmd).map_err(Sx126xError::Transport)?;

        self.write_cmd_data(CMD_SET_TX, &[0x00, 0x00, 0x00])?;

        for _ in 0..100_000 {
            let mut irq = [0u8; 2];
            self.read_cmd(CMD_GET_IRQ_STATUS, &mut irq)?;
            if irq[1] & 0x01 != 0 {
                self.write_cmd_data(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;
                return Ok(());
            }
        }
        Err(Sx126xError::Timeout)
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error> {
        self.write_cmd_data(CMD_SET_RX, &[0x00, 0x00, 0x00])?;

        let mut irq = [0u8; 2];
        self.read_cmd(CMD_GET_IRQ_STATUS, &mut irq)?;
        if irq[1] & 0x02 == 0 {
            return Ok(None);
        }
        self.write_cmd_data(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;

        let mut buf_status = [0u8; 2];
        self.read_cmd(CMD_GET_RX_BUFFER_STATUS, &mut buf_status)?;
        let payload_len = buf_status[0] as usize;
        let buf_offset = buf_status[1];

        let mut read_cmd = [CMD_READ_BUFFER, buf_offset, 0x00];
        self.spi.transfer_in_place(&mut read_cmd).map_err(Sx126xError::Transport)?;

        let mut payload = heapless::Vec::<u8, 240>::new();
        let read_len = payload_len.min(240);
        payload.resize(read_len, 0).ok();
        self.spi.read(payload.as_mut_slice()).map_err(Sx126xError::Transport)?;

        Ok(Some(ReceivedPacket { src_addr: 0, rssi: None, payload }))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use embedded_hal_mock::eh1::{
        pin::{Mock as PinMock, State, Transaction as PinTx},
        spi::{Mock as SpiMock, Transaction as SpiTx},
    };

    fn busy_ready() -> PinMock {
        PinMock::new(&[PinTx::get(State::Low)])
    }

    #[test]
    fn test_send_command_writes_bytes() {
        let spi = SpiMock::<u8>::new(&[
            SpiTx::transaction_start(),
            SpiTx::write_vec(std::vec![0x80, 0x00]),
            SpiTx::transaction_end(),
        ]);
        let busy = busy_ready();
        let reset = PinMock::new(&[]);

        let mut radio = Sx126xSpi::new(spi, busy, reset);
        radio.send_command(&[0x80, 0x00]).unwrap();

        radio.spi.done();
        radio.busy.done();
        radio.reset.done();
    }

    #[test]
    fn test_rf_frequency_calculation() {
        let rf_freq = Sx126xSpi::<SpiMock<u8>, PinMock, PinMock>::freq_to_register(868);
        assert!(rf_freq > 0x3600_0000 && rf_freq < 0x3700_0000);
    }
}
