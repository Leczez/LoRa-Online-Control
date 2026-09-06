//! Battery voltage/percentage sensing via a resistor divider from the LiPo
//! cell to an ADC1-capable GPIO (GPIO4 — free, not used by SPI/OLED/DIO0,
//! see the wiring doc).
//!
//! **Placeholder divider ratio — the physical circuit isn't built yet** (the
//! divider is being wired separately, not by this code). Assumes a 2:1
//! divider (e.g. two equal resistors, battery+ -> R -> ADC pin -> R -> GND)
//! so a 4.2V full battery reads ~2.1V at the pin, comfortably inside the
//! ADC's usable range at 11dB attenuation. Change DIVIDER_RATIO if the
//! actual resistor values differ.
//!
//! Percentage is a rough linear approximation between 3.0V (0%) and 4.2V
//! (100%) — real LiPo discharge curves are notably non-linear (flatter in
//! the middle), so treat this as "roughly how much is left," not a
//! calibrated fuel gauge.

use esp_idf_hal::adc::attenuation::DB_11;
use esp_idf_hal::adc::oneshot::config::{AdcChannelConfig, Calibration};
use esp_idf_hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_hal::adc::ADC1;
use esp_idf_hal::gpio::Gpio4;

/// Battery millivolts = ADC-pin millivolts * DIVIDER_RATIO.
const DIVIDER_RATIO: u32 = 2;

const EMPTY_MV: u32 = 3000;
const FULL_MV: u32 = 4200;

pub struct BatteryMonitor<'d> {
    channel: AdcChannelDriver<'d, Gpio4, AdcDriver<'d, ADC1>>,
}

impl<'d> BatteryMonitor<'d> {
    pub fn new(adc1: ADC1, pin: Gpio4) -> anyhow::Result<Self> {
        let driver = AdcDriver::new(adc1)?;
        let config = AdcChannelConfig { attenuation: DB_11, calibration: Calibration::Curve, ..Default::default() };
        let channel = AdcChannelDriver::new(driver, pin, &config)?;
        Ok(Self { channel })
    }

    /// Returns `(percent 0-100, battery millivolts)` — millivolts already
    /// corrected for `DIVIDER_RATIO`, i.e. the actual battery voltage, not
    /// the raw ADC-pin reading.
    pub fn read(&mut self) -> anyhow::Result<(u8, u16)> {
        let pin_mv = self.channel.read()? as u32;
        let battery_mv = (pin_mv * DIVIDER_RATIO).min(u16::MAX as u32);
        let pct = if battery_mv <= EMPTY_MV {
            0
        } else if battery_mv >= FULL_MV {
            100
        } else {
            ((battery_mv - EMPTY_MV) * 100 / (FULL_MV - EMPTY_MV)) as u8
        };
        Ok((pct, battery_mv as u16))
    }
}
