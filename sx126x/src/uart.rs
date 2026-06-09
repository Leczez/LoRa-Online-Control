// sx126x/src/uart.rs

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_io::{Error as _, Read, ReadExactError, Write};
use heapless::Vec;

use crate::{Config, LoraRadio, ReceivedPacket, Sx126xError};

pub struct Sx126xUart<UART, M0, M1, DELAY> {
    pub(crate) serial: UART,
    pub(crate) m0: M0,
    pub(crate) m1: M1,
    pub(crate) delay: DELAY,
    pub(crate) config: Config,
    /// Raw 12-byte ACK echoed by the module after the last configure() call.
    pub last_configure_ack: [u8; 12],
}

impl<UART, M0, M1, DELAY> Sx126xUart<UART, M0, M1, DELAY>
where
    UART: Read + Write,
    M0: OutputPin,
    M1: OutputPin,
    DELAY: DelayNs,
{
    pub fn new(serial: UART, m0: M0, m1: M1, delay: DELAY) -> Self {
        Self {
            serial,
            m0,
            m1,
            delay,
            last_configure_ack: [0u8; 12],
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
        // E22 needs ~500ms to stabilize after entering config mode before it accepts commands.
        self.delay.delay_ms(500);
        Ok(())
    }

    fn enter_normal_mode(&mut self) -> Result<(), Sx126xError<UART::Error>> {
        self.m0.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.m1.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.delay.delay_ms(200);
        Ok(())
    }

    /// Configure the module only if its current flash registers differ from `config`.
    /// Returns `true` if a write was performed, `false` if already correct.
    /// On success, `last_configure_ack` holds the 12-byte read-back regardless.
    pub fn configure_if_needed(&mut self, config: &Config) -> Result<bool, Sx126xError<UART::Error>> {
        let desired = config.to_registers_checked().ok_or(Sx126xError::InvalidConfig)?;
        let current = self.read_config()?;
        if current[3..] == desired[3..] {
            self.last_configure_ack = current;
            return Ok(false);
        }
        self.configure(config)?;
        Ok(true)
    }

    /// Read 9 registers starting at address 0 (addresses, net_id, speed, power, channel, option, crypt).
    /// Returns raw 12-byte response: [0xC1, 0x00, 0x09, REG0..REG8].
    pub fn read_config(&mut self) -> Result<[u8; 12], Sx126xError<UART::Error>> {
        self.enter_config_mode()?;
        // Drain any bytes emitted during mode transition before issuing read command.
        let mut discard = [0u8; 1];
        loop {
            match self.serial.read(&mut discard) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        self.serial
            .write_all(&[0xC1, 0x00, 0x09])
            .map_err(Sx126xError::Transport)?;
        let mut buf = [0u8; 12];
        self.serial.read_exact(&mut buf).map_err(|e| match e {
            ReadExactError::UnexpectedEof => Sx126xError::Timeout,
            ReadExactError::Other(inner) => Sx126xError::Transport(inner),
        })?;
        self.enter_normal_mode()?;
        Ok(buf)
    }
}

impl<UART, M0, M1, DELAY> LoraRadio for Sx126xUart<UART, M0, M1, DELAY>
where
    UART: Read + Write,
    M0: OutputPin,
    M1: OutputPin,
    DELAY: DelayNs,
{
    type Error = Sx126xError<UART::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        let regs = config
            .to_registers_checked()
            .ok_or(Sx126xError::InvalidConfig)?;

        self.config = config.clone();
        self.enter_normal_mode()?;
        self.enter_config_mode()?;

        // Flush any bytes the module emitted during mode transitions before issuing command.
        let mut discard = [0u8; 1];
        loop {
            match self.serial.read(&mut discard) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }

        self.serial
            .write_all(&regs)
            .map_err(Sx126xError::Transport)?;

        let mut ack = [0u8; 12];
        self.serial.read_exact(&mut ack).map_err(|e| match e {
            ReadExactError::UnexpectedEof => Sx126xError::Timeout,
            ReadExactError::Other(inner) => Sx126xError::Transport(inner),
        })?;

        if ack[0] != 0xC1 {
            return Err(Sx126xError::Timeout);
        }

        self.last_configure_ack = ack;
        self.enter_normal_mode()?;

        // Drain any bytes the module output during mode transitions.
        let mut discard = [0u8; 1];
        loop {
            match self.serial.read(&mut discard) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }

        Ok(())
    }

    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error> {
        self.enter_normal_mode()?;
        let routing = [
            (dest >> 8) as u8,
            (dest & 0xFF) as u8,
            self.config.freq_offset_byte(),
        ];
        // Prepend our own address so the receiver knows the source.
        // The E22 strips the routing header on TX; the receiver sees [src_high, src_low, data...].
        let src_prefix = [
            (self.config.addr >> 8) as u8,
            (self.config.addr & 0xFF) as u8,
        ];
        self.serial
            .write_all(&routing)
            .map_err(Sx126xError::Transport)?;
        self.serial
            .write_all(&src_prefix)
            .map_err(Sx126xError::Transport)?;
        self.serial
            .write_all(payload)
            .map_err(Sx126xError::Transport)?;
        self.serial.flush().map_err(Sx126xError::Transport)?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error> {
        let mut header = [0u8; 2];
        match self.serial.read_exact(&mut header) {
            Err(ReadExactError::Other(e))
                if e.kind() == embedded_io::ErrorKind::TimedOut =>
            {
                return Ok(None);
            }
            Err(ReadExactError::UnexpectedEof) => return Ok(None),
            Err(ReadExactError::Other(e)) => return Err(Sx126xError::Transport(e)),
            Ok(()) => {}
        }

        let src_addr = ((header[0] as u16) << 8) | header[1] as u16;

        let mut body = Vec::<u8, 240>::new();
        let mut byte = [0u8; 1];
        loop {
            match self.serial.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => { body.push(byte[0]).ok(); }
                Err(e) if e.kind() == embedded_io::ErrorKind::TimedOut => break,
                Err(e) => return Err(Sx126xError::Transport(e)),
            }
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

    struct NoopDelay;

    impl embedded_hal::delay::DelayNs for NoopDelay {
        fn delay_ns(&mut self, _ns: u32) {}
    }

    struct MockSerial {
        write_buf: std::vec::Vec<u8>,
        /// Bytes available immediately (e.g., ambient data, received packets).
        read_buf: std::collections::VecDeque<u8>,
        /// Bytes that become readable only after the first write (e.g., command ACK).
        response_buf: std::collections::VecDeque<u8>,
    }

    impl MockSerial {
        fn new(read_data: &[u8]) -> Self {
            Self {
                write_buf: std::vec::Vec::new(),
                read_buf: read_data.iter().copied().collect(),
                response_buf: std::collections::VecDeque::new(),
            }
        }

        fn new_with_response(initial: &[u8], response: &[u8]) -> Self {
            Self {
                write_buf: std::vec::Vec::new(),
                read_buf: initial.iter().copied().collect(),
                response_buf: response.iter().copied().collect(),
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
            // On first write, move response bytes into the read buffer.
            if !self.response_buf.is_empty() {
                self.read_buf.extend(self.response_buf.drain(..));
            }
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
        let m0_expects = std::vec![PinTx::set(State::Low), PinTx::set(State::Low), PinTx::set(State::Low)];
        let m1_expects = std::vec![PinTx::set(State::Low), PinTx::set(State::High), PinTx::set(State::Low)];

        let serial = MockSerial::new_with_response(&[], &config_ack());
        let m0 = PinMock::new(&m0_expects);
        let m1 = PinMock::new(&m1_expects);

        let mut radio = Sx126xUart::new(serial, m0, m1, NoopDelay);
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

        let mut radio = Sx126xUart { serial, m0, m1, delay: NoopDelay, config: config(), last_configure_ack: [0u8; 12] };
        radio.send(1, b"hello").unwrap();

        let freq_off = config().freq_offset_byte();
        // routing [dest_h, dest_l, ch] + src_prefix [addr_h, addr_l] + payload
        let expected = [0x00, 0x01, freq_off, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(radio.serial.write_buf, expected);

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_returns_none_when_empty() {
        let serial = MockSerial::new(&[]);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, delay: NoopDelay, config: config(), last_configure_ack: [0u8; 12] };
        let result = radio.receive().unwrap();
        assert!(result.is_none());

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_parses_packet_with_rssi() {
        // rssi = -(256 - 174) = -82 dBm
        // 2-byte src_addr prefix (as embedded by sender), then payload + rssi byte
        let packet = [0x00, 0x01, b'h', b'i', 174u8];
        let serial = MockSerial::new(&packet);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, delay: NoopDelay, config: config(), last_configure_ack: [0u8; 12] };
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
        let packet = [0x00, 0x02, b'o', b'k'];
        let serial = MockSerial::new(&packet);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, delay: NoopDelay, config: cfg, last_configure_ack: [0u8; 12] };
        let pkt = radio.receive().unwrap().unwrap();

        assert_eq!(pkt.src_addr, 2);
        assert_eq!(pkt.rssi, None);
        assert_eq!(pkt.payload.as_slice(), b"ok");

        radio.m0.done();
        radio.m1.done();
    }
}
