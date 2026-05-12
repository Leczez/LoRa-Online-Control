// sx126x/src/uart.rs

use embedded_hal::digital::OutputPin;
use embedded_io::{Read, Write};
use heapless::Vec;

use crate::{Config, LoraRadio, ReceivedPacket, Sx126xError};

pub struct Sx126xUart<UART, M0, M1> {
    pub(crate) serial: UART,
    pub(crate) m0: M0,
    pub(crate) m1: M1,
    pub(crate) config: Config,
}

impl<UART, M0, M1> Sx126xUart<UART, M0, M1>
where
    UART: Read + Write,
    M0: OutputPin,
    M1: OutputPin,
{
    pub fn new(serial: UART, m0: M0, m1: M1) -> Self {
        Self {
            serial,
            m0,
            m1,
            config: Config {
                freq_mhz: 868,
                addr: 0,
                net_id: 0,
                power: crate::TxPower::Dbm22,
                air_speed: crate::AirSpeed::Bps2400,
                buffer_size: crate::BufferSize::Bytes240,
                rssi: false,
                crypt: 0,
            },
        }
    }

    fn enter_config_mode(&mut self) -> Result<(), Sx126xError<UART::Error>> {
        self.m0.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.m1.set_high().map_err(|_| Sx126xError::InvalidConfig)?;
        Ok(())
    }

    fn enter_normal_mode(&mut self) -> Result<(), Sx126xError<UART::Error>> {
        self.m0.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.m1.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        Ok(())
    }
}

impl<UART, M0, M1> LoraRadio for Sx126xUart<UART, M0, M1>
where
    UART: Read + Write,
    M0: OutputPin,
    M1: OutputPin,
{
    type Error = Sx126xError<UART::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        let regs = config
            .to_registers_checked()
            .ok_or(Sx126xError::InvalidConfig)?;

        self.config = config.clone();
        self.enter_config_mode()?;

        self.serial
            .write_all(&regs)
            .map_err(Sx126xError::Transport)?;

        let mut ack = [0u8; 12];
        self.serial
            .read(&mut ack)
            .map_err(Sx126xError::Transport)?;

        if ack[0] != 0xC1 {
            return Err(Sx126xError::Timeout);
        }

        self.enter_normal_mode()?;
        Ok(())
    }

    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error> {
        self.enter_normal_mode()?;
        let freq_off = self.config.freq_offset_byte();
        let header = [
            (dest >> 8) as u8,
            (dest & 0xFF) as u8,
            freq_off,
            (self.config.addr >> 8) as u8,
            (self.config.addr & 0xFF) as u8,
            freq_off,
        ];
        self.serial
            .write_all(&header)
            .map_err(Sx126xError::Transport)?;
        self.serial
            .write_all(payload)
            .map_err(Sx126xError::Transport)?;
        self.serial.flush().map_err(Sx126xError::Transport)?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error> {
        let mut header = [0u8; 3];
        let n = self.serial.read(&mut header).map_err(Sx126xError::Transport)?;
        if n == 0 {
            return Ok(None);
        }

        let src_addr = ((header[0] as u16) << 8) | header[1] as u16;

        let mut body = Vec::<u8, 240>::new();
        let mut byte = [0u8; 1];
        loop {
            let n = self.serial.read(&mut byte).map_err(Sx126xError::Transport)?;
            if n == 0 {
                break;
            }
            body.push(byte[0]).ok();
        }

        let (payload_bytes, rssi) = if self.config.rssi && !body.is_empty() {
            let rssi_raw = *body.last().unwrap();
            let rssi_dbm = -(256i16 - rssi_raw as i16);
            (&body[..body.len() - 1], Some(rssi_dbm))
        } else {
            (body.as_slice(), None)
        };

        let mut payload = Vec::<u8, 240>::new();
        payload.extend_from_slice(payload_bytes).ok();

        Ok(Some(ReceivedPacket { src_addr, rssi, payload }))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::config::*;
    use embedded_hal_mock::eh1::pin::{Mock as PinMock, State, Transaction as PinTx};

    struct MockSerial {
        write_buf: std::vec::Vec<u8>,
        read_buf: std::collections::VecDeque<u8>,
    }

    impl MockSerial {
        fn new(read_data: &[u8]) -> Self {
            Self {
                write_buf: std::vec::Vec::new(),
                read_buf: read_data.iter().copied().collect(),
            }
        }
    }

    impl embedded_io::ErrorType for MockSerial {
        type Error = core::convert::Infallible;
    }

    impl embedded_io::Read for MockSerial {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let n = buf.len().min(self.read_buf.len());
            for b in buf.iter_mut().take(n) {
                *b = self.read_buf.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl embedded_io::Write for MockSerial {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> Result<(), Self::Error> { Ok(()) }
    }

    fn config() -> Config {
        Config {
            freq_mhz: 868,
            addr: 0,
            net_id: 0,
            power: TxPower::Dbm22,
            air_speed: AirSpeed::Bps2400,
            buffer_size: BufferSize::Bytes240,
            rssi: true,
            crypt: 0,
        }
    }

    fn config_ack() -> [u8; 12] {
        let mut ack = [0u8; 12];
        ack[0] = 0xC1;
        ack
    }

    #[test]
    fn test_configure_writes_correct_registers() {
        let m0_expects = std::vec![PinTx::set(State::Low), PinTx::set(State::Low)];
        let m1_expects = std::vec![PinTx::set(State::High), PinTx::set(State::Low)];

        let serial = MockSerial::new(&config_ack());
        let m0 = PinMock::new(&m0_expects);
        let m1 = PinMock::new(&m1_expects);

        let mut radio = Sx126xUart::new(serial, m0, m1);
        radio.configure(&config()).unwrap();

        let expected_regs = config().to_registers();
        assert_eq!(&radio.serial.write_buf[..12], &expected_regs);

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_send_writes_correct_packet() {
        let serial = MockSerial::new(&[]);
        let m0 = PinMock::new(&[PinTx::set(State::Low)]);
        let m1 = PinMock::new(&[PinTx::set(State::Low)]);

        let mut radio = Sx126xUart { serial, m0, m1, config: config() };
        radio.send(1, b"hello").unwrap();

        let freq_off = config().freq_offset_byte();
        let expected = [0x00, 0x01, freq_off, 0x00, 0x00, freq_off,
                        b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(radio.serial.write_buf, expected);

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_returns_none_when_empty() {
        let serial = MockSerial::new(&[]);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, config: config() };
        let result = radio.receive().unwrap();
        assert!(result.is_none());

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_parses_packet_with_rssi() {
        // rssi = -(256 - 174) = -82 dBm
        let packet = [0x00, 0x01, 0x12, b'h', b'i', 174u8];
        let serial = MockSerial::new(&packet);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, config: config() };
        let pkt = radio.receive().unwrap().unwrap();

        assert_eq!(pkt.src_addr, 1);
        assert_eq!(pkt.rssi, Some(-82));
        assert_eq!(pkt.payload.as_slice(), b"hi");

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_parses_packet_without_rssi() {
        let mut cfg = config();
        cfg.rssi = false;
        let packet = [0x00, 0x02, 0x12, b'o', b'k'];
        let serial = MockSerial::new(&packet);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, config: cfg };
        let pkt = radio.receive().unwrap().unwrap();

        assert_eq!(pkt.src_addr, 2);
        assert_eq!(pkt.rssi, None);
        assert_eq!(pkt.payload.as_slice(), b"ok");

        radio.m0.done();
        radio.m1.done();
    }
}
