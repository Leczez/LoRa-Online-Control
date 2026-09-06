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

## RFM95W (SX1276) SPI wiring — as flashed in `esp32-node`

| Signal | GPIO | Notes |
|---|---|---|
| SCK | 12 | |
| MOSI | 11 | |
| MISO | 13 | |
| NSS (CS) | 10 | |
| RESET | 9 | |
| DIO0 (interrupt) | 14 | Wired into the firmware (`Sx127xSpi::new_with_dio0` path exists) but **not yet physically connected** on the bench unit — firmware currently falls back to SPI-register polling for TX/CAD completion. Connect this pin and switch `esp32-node/src/main.rs` over to `new_with_dio0` for cheaper GPIO-based waiting. |

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
