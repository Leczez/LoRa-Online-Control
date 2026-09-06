# diymore ESP32-S3 mini dev board — RFM95W wiring

Board silkscreen reads **ESP32-S3-Zero** (diymore's listing resells this
common clone design). Chip is the bare **ESP32-S3FH4R2** (in-package 4MB
flash + 2MB quad-SPI PSRAM), not a WROOM module — see the pin-definition
photo in this folder (`71+dC51brFL._AC_SL1500_.jpg`, from the Amazon
listing) for the board's full silkscreen pinout.

Confirmed against Espressif's own reserved-pin list (not the board photo's
own annotations, which turned out to be inconsistent with the actual
ESP32-S3FH4R2 datasheet in a couple of places): strapping pins are GPIO0/3/
45/46, native USB-OTG is fixed at GPIO19/20, and SPI flash/PSRAM reserve
GPIO26-32 (GPIO33-37 only matter in octal mode, which this chip doesn't
use). None of the assignments below conflict with any of those.

**Physical accessibility, separate from electrical reservations:** GPIO14/
15/16 are broken out on this board only as small SMD probe points clustered
near a component labeled "C3" close to the chip — not proper header pins.
Nothing electrically wrong with them, but not realistically solderable for
a wired connection, so avoid them for anything you actually need to wire.

## RFM95W (SX1276) SPI wiring — as flashed in `esp32-node`

| Signal | GPIO | Notes |
|---|---|---|
| SCK | 12 | |
| MOSI | 11 | |
| MISO | 13 | |
| NSS (CS) | 10 | |
| RESET | 9 | |
| DIO0 (interrupt) | 7 | Not GPIO14 — see the SMD-probe-point note above. Physically connected; `main.rs` uses `Sx127xSpi::new_with_dio0` for TX/CAD completion instead of SPI-register polling. |

## Battery sensing

| Signal | GPIO | Notes |
|---|---|---|
| Battery sense (ADC1) | 4 | **Placeholder** — `esp32-node/src/battery.rs` assumes a 2:1 resistor divider (e.g. two equal resistors, battery+ → R → this pin → R → GND) so a 4.2V full battery reads ~2.1V at the pin. Adjust `DIVIDER_RATIO` in that file if the actual resistor values differ. Divider not built yet as of this writing. |

## Other assignments (planned, not yet wired)

| Signal | GPIO | Notes |
|---|---|---|
| OLED SDA | 8 | Front header |
| OLED SCL | 18 | Back-side castellated pad, not the front header — needs soldering to the underside |
| Config switch | 6 | Superseded — see below |

Note: the physical config switch originally planned here has been dropped
in favor of a timed Wi-Fi config portal (no switch, no dedicated GPIO) —
see `esp32-node/src/wifi_config.rs`. GPIO6 is free again.

## USB (SI master link)

GPIO19/20 (native USB D-/D+) are **not broken out separately** on this
board — they go straight to the single onboard USB-C connector. There is
no second USB-C port to dedicate purely to host mode; see
`docs/superpowers/specs/2026-08-25-esp32-si-punch-node-design.md` and this
project's chat history for the resulting "dual-role on one port" plan
(flash/debug via PC with the buck-boost switched off, SI master via a
USB-A-to-C adapter with the buck-boost switched on).
