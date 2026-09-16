# ESP32-S3-WROOM-2 — module reference for custom PCB design

Reference notes for designing a custom PCB around the ESP32-S3-WROOM-2
module (candidate replacement for the diymore ESP32-S3-Zero clone
documented alongside this file — see `../diymore_esp32-s3_mini_dev_board/
wiring.md`). Source: [Espressif datasheet v1.7](https://documentation.espressif.com/esp32-s3-wroom-2_datasheet_en.pdf).

Unlike the diymore board's bare ESP32-S3FH4R2 chip, the WROOM-2 is a
**module**: crystal, Octal SPI flash (16/32MB), Octal PSRAM (8/16MB), and
RF matching/PCB antenna are all integrated. No external crystal or RF
matching network design is needed — only power, strapping, and the GPIOs
you actually use.

Variants: `ESP32-S3-WROOM-2-N32R16V` (32MB flash / 16MB PSRAM) is the
current part; N16R8V and N32R8V are EOL. All variants: 18.0×25.5×3.1mm,
operating temp −40~65°C (85°C if PSRAM ECC enabled, at reduced usable
PSRAM size).

## Power

- 3.3V supply, **≥500mA** capability (peak draw ~355mA during TX at
  802.11b 1Mbps/20.5dBm).
- Decoupling per Espressif's reference schematic: 22µF + 0.1µF near the
  3V3 pin.
- **EN (chip enable/reset) pin needs an RC delay**: R=10kΩ (3V3→EN),
  C=1µF (EN→GND). Ensures power rails stabilize (≥50µs) before EN goes
  high. Do not leave EN floating.
- EPAD (pin 41, exposed thermal pad) → GND. Soldering it is optional
  (thermal performance only).

## Strapping pins — required for correct boot

| Pin | Default | Function |
|---|---|---|
| GPIO0 | weak pull-up (=1) | Boot mode select (with GPIO46). =1 → SPI Boot (normal run). Pull low at reset → download mode. Avoid large caps on this pin (risk of false download-mode entry). |
| GPIO46 | weak pull-down (=0) | Boot mode select (with GPIO0). ROM message printing control. |
| GPIO45 | weak pull-down (=0) | VDD_SPI voltage select — leave at default unless you know you need 1.8V flash logic. |
| GPIO3 | **no internal pull, floating by default** | JTAG signal source select. Reference schematic leaves it unpopulated (TBD footprint); fine to leave unconnected for default USB-Serial-JTAG operation. |

Put a momentary **BOOT button** (GPIO0→GND) and a momentary **RESET
button** (EN→GND) on the board — needed to force download mode for
flashing without relying on auto-reset circuitry.

## USB — GPIO19/20 share a single PHY, two peripherals contend for it

GPIO19 = USB D−, GPIO20 = USB D+, hardwired to the internal USB PHY (not
routable to other pins). Two peripherals share this PHY and only one can
be active at a time, selected in firmware:

- **USB-Serial-JTAG controller** (device-only, default) — what `espflash`/
  `esptool`/the USB console use.
- **USB-OTG controller** (host or device, via TinyUSB) — used for USB
  host-mode application features.

If firmware puts the chip into USB Host mode, the port is not
simultaneously flashable as a device. This project's existing firmware
works around it by delaying the host-mode switch ~10s after boot (see
`esp32-node` commit history) — that only works when the connector itself
is device-capable (e.g. plugged into a PC during the window), not when
GPIO19/20 are wired straight to a host-only USB-A receptacle.

**Flashing paths independent of GPIO19/20** (use these if the native USB
port is dedicated to host mode on the new PCB):

- **UART0** (pins 36/37, U0RXD/U0TXD) — put a USB-UART bridge (CP2102N or
  CH340C) with its own USB-C jack on the board, or just break out a 6-pin
  header (3V3, GND, EN, GPIO0, U0TXD, U0RXD) for an external FTDI/CP2102
  dongle. Add the classic two-transistor auto-reset circuit (DTR→GPIO0,
  RTS→EN) if you want `esptool` to reset-into-bootloader automatically.
- **JTAG** via MTMS/MTDI/MTDO/MTCK = IO42/41/40/39 — needs an external
  JTAG probe (ESP-Prog etc.), also fully independent of GPIO19/20.

GPIO0-low + reset forces download mode regardless of USB Host state —
UART0/JTAG flashing is orthogonal to whatever GPIO19/20 are doing.

Precedent: Espressif's own ESP32-S3-USB-OTG dev board uses a hardware
analog USB switch (GPIO18-controlled) to route GPIO19/20 between a host
connector and a device connector, plus a *third*, fully separate
USB-UART-bridge connector dedicated purely to flashing — GPIO19/20 never
contend with the flashing path on that board.

## Pin notes

- 41 pins total (40 castellated + EPAD). Full table in the datasheet
  §3.2; key non-obvious ones:
  - Pins 28–30: NC (no connect).
  - GPIO33–37: **do not use** — reserved for Octal PSRAM/flash on this
    module (unlike the diymore board's non-octal chip, where those pins
    only mattered in octal mode).
  - IO47/IO48 operate in the 1.8V domain (VDD_SPI-tied), not 3.3V like
    other GPIOs — avoid using these for anything expecting 3.3V logic
    levels.
- UART0 default: U0TXD=pin37(IO43), U0RXD=pin36(IO44). Add a 499Ω series
  resistor on U0TXD if the run to a header/connector is long (harmonic
  suppression) — the module's own internal schematic already does this
  on-die, this is belt-and-suspenders for external routing.

## Physical / antenna keepout

- 18.0×25.5×3.1mm, 40 pins at 1.27mm pitch (0.85mm pitch on the 15/26
  edge), 0.9mm pad width.
- On-board PCB trace antenna occupies the top ~7.5mm of the module's
  18mm width. **Keep this area free of copper/ground-plane/components on
  all base-board layers** — no traces or pour underneath or in front of
  it, and no metal enclosure directly over it.

## KiCad

Official symbol + footprint + STEP model:
[espressif/kicad-libraries](https://github.com/espressif/kicad-libraries),
`footprints/Espressif.pretty/ESP32-S3-WROOM-2.kicad_mod`. Install via
KiCad's Plugin & Content Manager, or add the repo as an extra library
table entry. Not included in Arch's `kicad-library` package (that's
KiCad's own generic parts) — needs adding separately.
