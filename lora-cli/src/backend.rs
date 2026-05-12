// lora-cli/src/backend.rs

use anyhow::Result;
use sx126x::{Config, NoPin, ReceivedPacket, Sx126xUart, LoraRadio};

use crate::Args;

struct StdDelay;

impl embedded_hal::delay::DelayNs for StdDelay {
    fn delay_ns(&mut self, ns: u32) {
        std::thread::sleep(std::time::Duration::from_nanos(ns as u64));
    }
}

/// Wraps serialport to implement embedded_io::Read + Write.
///
/// `embedded_io::ErrorType::Error` must implement `embedded_io::Error`, which
/// `std::io::Error` does not. We use `embedded_io::ErrorKind` as the error type
/// and convert `std::io::Error` via the `From<std::io::ErrorKind>` impl.
pub struct SerialPortWrapper(pub Box<dyn serialport::SerialPort>);

impl embedded_io::ErrorType for SerialPortWrapper {
    type Error = embedded_io::ErrorKind;
}

impl embedded_io::Read for SerialPortWrapper {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, Self::Error> {
        std::io::Read::read(&mut self.0, buf)
            .map_err(|e| e.kind().into())
    }
}

impl embedded_io::Write for SerialPortWrapper {
    fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Self::Error> {
        std::io::Write::write(&mut self.0, buf)
            .map_err(|e| e.kind().into())
    }
    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        std::io::Write::flush(&mut self.0)
            .map_err(|e| e.kind().into())
    }
}

/// Error-erased radio interface used by the event loop.
pub trait Radio: Send {
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()>;
    fn receive(&mut self) -> Result<Option<ReceivedPacket>>;
}

impl<R: LoraRadio + Send> Radio for R
where
    R::Error: std::error::Error + Send + Sync + 'static,
{
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()> {
        LoraRadio::send(self, dest, payload).map_err(anyhow::Error::from)
    }
    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        LoraRadio::receive(self).map_err(anyhow::Error::from)
    }
}

fn build_config(args: &Args) -> Result<Config> {
    use sx126x::{AirSpeed, BufferSize, TxPower};

    let power = match args.power {
        22 => TxPower::Dbm22,
        17 => TxPower::Dbm17,
        13 => TxPower::Dbm13,
        10 => TxPower::Dbm10,
        p  => anyhow::bail!("unsupported power {}dBm — use 10, 13, 17, or 22", p),
    };
    let air_speed = match args.air_speed {
        1200  => AirSpeed::Bps1200,
        2400  => AirSpeed::Bps2400,
        4800  => AirSpeed::Bps4800,
        9600  => AirSpeed::Bps9600,
        19200 => AirSpeed::Bps19200,
        38400 => AirSpeed::Bps38400,
        62500 => AirSpeed::Bps62500,
        s => anyhow::bail!("unsupported air_speed {} bps", s),
    };
    Ok(Config {
        freq_mhz: args.freq,
        addr: args.addr,
        net_id: 0,
        power,
        air_speed,
        buffer_size: BufferSize::Bytes240,
        rssi: true,
        crypt: 0,
    })
}

fn open_serial(port: &str) -> Result<SerialPortWrapper> {
    let s = serialport::new(port, 9600)
        .timeout(std::time::Duration::from_millis(100))
        .open()
        .map_err(|e| anyhow::anyhow!("failed to open {}: {}", port, e))?;
    Ok(SerialPortWrapper(s))
}

/// Entry point called from main. Builds backend and launches TUI.
pub fn run(args: Args) -> Result<()> {
    let config = build_config(&args)?;

    #[cfg(feature = "rpi")]
    if args.m0_pin.is_some() && args.m1_pin.is_some() {
        return run_rpi(args, config);
    }

    run_desktop(args, config)
}

fn run_desktop(args: Args, config: Config) -> Result<()> {
    let serial = open_serial(&args.port)?;
    let mut driver = Sx126xUart::new(serial, NoPin, NoPin, StdDelay);
    driver.configure(&config)?;
    crate::ui::run_app(&args.port, args.dest, config, Box::new(driver))
}

#[cfg(feature = "rpi")]
fn run_rpi(args: Args, config: Config) -> Result<()> {
    use rppal::gpio::Gpio;
    use rppal::uart::{Parity, Uart};

    struct RppalSerial(Uart);

    impl embedded_io::ErrorType for RppalSerial {
        type Error = std::io::Error;
    }
    impl embedded_io::Read for RppalSerial {
        fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, Self::Error> {
            self.0.read(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }
    }
    impl embedded_io::Write for RppalSerial {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Self::Error> {
            self.0.write(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }
        fn flush(&mut self) -> std::result::Result<(), Self::Error> { Ok(()) }
    }

    let gpio = Gpio::new()?;
    let m0 = gpio.get(args.m0_pin.unwrap())?.into_output();
    let m1 = gpio.get(args.m1_pin.unwrap())?.into_output();
    let uart = Uart::with_path(&args.port, 9600, Parity::None, 8, 1)?;

    let mut driver = Sx126xUart::new(RppalSerial(uart), m0, m1, StdDelay);
    driver.configure(&config)?;
    crate::ui::run_app(&args.port, args.dest, config, Box::new(driver))
}
