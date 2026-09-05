# ESP32 SportIdent Punch Node — Design

**Date:** 2026-08-25

## 1. Goals & scope

**Goal:** Field units (ESP32-S3 + RFM95W) read punches from a USB-connected SportIdent master station and relay them over LoRa directly to the RPi's bare SX1276/RFM95W module, for start/check control points that can't run a wired PC.

**In scope:**
- New `sportident-proto` shared crate: protocol parsing extracted from `lora-server/src/sportident.rs`, `no_std`-compatible, used by both the RPi daemon (`lora-server`) and the ESP32 firmware.
- The `sx127x` driver crate for the RFM95W/SX1276 (`no_std`, generic over `embedded-hal`) already exists and is what `lora-server` itself runs on the RPi — the ESP32 firmware uses the exact same crate, not a new one.
- New ESP32-S3 firmware binary: USB-host reads the SI master, forwards punches over LoRa.
- RPi-side change: recognize incoming `PUNCH ...` payloads from remote nodes and render them the same way local punches already are (`LogEntry::SiPunch`), instead of as generic RX text.
- Multiple simultaneous field nodes, each with a distinct LoRa address.
- Field-editable node address/dest/channel via a switch-gated Wi-Fi config page (no reflashing).
- A small I2C OLED status display — link/RSSI, SI reader connected/not, last punch sent, node address — so a technician can verify a control point is working without a laptop.

**Out of scope for this design:**
- The SF/BW/CR + over-the-air frame format discovery spike itself — exploratory work, not something to design up front. It's an early task in the implementation plan, with a documented fallback (section 4) if it doesn't pan out.
- Battery/power management beyond the Wi-Fi-off-by-default switch (no deep sleep / WOR mode) — treating this as "on for the duration of an event," same as how SI stations are used today. The OLED fits this model too: no dedicated sleep scheduling, just dim/blank it between state changes rather than refreshing continuously (see component 6).
- Forwarding punches onward from the RPi to Tävlingsarenan/ROC — that's already the daemon's existing broadcast mechanism; no new work implied here.

## 2. Architecture & components

```
 Control point A                Control point B
 ┌─────────────────────┐        ┌─────────────────────┐
 │ SI master (USB)      │        │ SI master (USB)      │
 │        │ USB host     │        │        │ USB host     │
 │ ESP32-S3 (addr=10)   │        │ ESP32-S3 (addr=11)   │        ...more nodes
 │        │ SPI          │        │        │ SPI          │
 │  RFM95W (SX1276)     │        │  RFM95W (SX1276)     │
 └──────────┬───────────┘        └──────────┬───────────┘
            │  LoRa (PUNCH <card_id> <punches>)         │
            └───────────────────┬────────────────────────┘
                                 ▼
                    RFM95W (SX1276, on RPi via SPI)
                                 │
                          lora-server daemon
                    (same sx127x driver as the nodes)
                                 │
                    parses "PUNCH ..." RX payloads →
                    LogEntry::SiPunch, broadcasts to
                    attached UI / onward to Tävlingsarenan
```

**Components:**

1. **`sportident-proto`** (new crate, `no_std`) — CRC16, packet framing, `parse_si9`/`parse_card_data`, `ControlPunch`/`CardReadout` types, `CardReadout::to_payload()`. Pulled out of today's `lora-server/src/sportident.rs`. No transport code lives here — that stays per-platform (`serialport` on the RPi/desktop side, ESP-IDF `usb_host` CDC-ACM on the ESP32 side).

2. **`sx127x`** (existing crate, `no_std`, generic over `embedded-hal` SPI) — minimal SX1276 driver: register setup for a given SF/BW/CR/sync-word/preamble, TX, RX, addressing/framing. Already used by `lora-server` on the RPi, so the ESP32 firmware talks the same over-the-air format by construction — no separate compatibility spike needed (see "Risk 1," now resolved, in section 4).

3. **ESP32-S3 firmware** (new binary crate, `esp-idf-hal`, std Rust) — a USB-host worker thread (FFI bindings to ESP-IDF's `usb_host`/CDC-ACM component) reads the SI master and feeds `sportident-proto`'s parser, producing `CardReadout`s over a channel; the main loop drains the channel and transmits each one via `sx127x`. Structurally mirrors `spawn_si_worker()` in today's `sportident.rs` — same shape, different transport underneath.

4. **`lora-server` (RPi) changes** — in `backend.rs`'s `run_daemon_loop` and `ui.rs`'s RX handling, when an incoming packet's payload starts with `PUNCH `, parse it into the same `LogEntry::SiPunch` used for locally-read punches instead of a generic RX log line.

5. **Config mode** — gated by a physical switch on a GPIO pin, checked once at boot:
   - **Switch off (normal):** boots straight into the USB-host + LoRa relay loop (component 3). No Wi-Fi radio ever powers on.
   - **Switch on (config):** skips normal operation entirely, starts a Wi-Fi AP + a minimal web page (addr/dest/channel fields), saves to NVS (ESP-IDF's flash key-value store) on submit. Node needs a reboot (or a "restart" button on the page) to pick up new values and re-enter normal mode. The OLED (component 6) shows the AP's SSID/IP while in this mode, so no second device is needed just to find it.
   - Firmware reads `addr`/`dest`/`channel` from NVS at boot in the normal path, falling back to sane defaults if unset (first boot).

6. **OLED status display** (new, small I2C SSD1306-class 0.96" panel) — shows, in normal operation: LoRa link status (last successful send/ack, RSSI to the base station), whether the SI master is currently connected over USB, the node's own address, and a brief "card `<id>` sent" confirmation on each punch. In config mode, shows the Wi-Fi AP's SSID/IP instead (see component 5). Not continuously redrawn — updates only on state change and otherwise sits static (the SSD1306 controller's own sleep mode can blank it between updates), keeping its contribution close to its active-draw figure rather than that figure held indefinitely.
   - **Power cost:** roughly 10–20mA while actively displaying content, under 1mA in the controller's sleep mode. Against an estimated ~150–250mA average for the ESP32-S3 + RFM95W node itself (USB host running, LoRa mostly idle/RX with occasional TX bursts at SF11/12), this is a ~5–10% addition to the power budget — small against the "on for the duration of an event" model already adopted for the node as a whole, not something that changes the event-length battery math meaningfully.

## 3. Data flow & addressing

**Per-punch flow:**
1. Card tap → SI master emits data over USB → ESP32's USB-host worker thread reads raw bytes → feeds `sportident-proto`'s parser → `CardReadout { card_id, punches }`.
2. `CardReadout::to_payload()` → `"PUNCH <card_id> <station>:<time>,..."` (same format already used for local punches today).
3. ESP32 main loop calls `sx127x_radio.send(rpi_addr, payload)` — same shape as the existing `LoraRadio::send(dest, payload)` trait, so the ESP32 code looks structurally identical to `backend.rs`'s existing send call.
4. RPi's SX1276 module receives over SPI → `Sx127xSpi::receive()` → `ReceivedPacket { src_addr, rssi, payload }` — no different from any other inbound packet today.
5. `run_daemon_loop` (and the standalone `ui.rs` RX path) checks `payload.starts_with("PUNCH ")`; if so, parses it into `LogEntry::SiPunch` for display, instead of a generic RX line. Falls through to today's generic handling otherwise.

**Addressing:**
- Each ESP32 node gets a distinct LoRa `addr` (e.g. 10, 11, 12...), set via the Wi-Fi config page (section 2, component 5) and persisted to NVS.
- All nodes transmit with `dest` = the RPi's configured address.
- The RPi tells nodes apart via `pkt.src_addr` on receive — already exposed by `ReceivedPacket`, no RPi-side change needed for that part; only the payload-parsing change in step 5 above is new.
- Channel/frequency must match across every node and the RPi (433 MHz, whatever channel offset the RPi's currently deployed at) — a deployment/config concern, not a code one.
- Address collisions between nodes are an operational mistake, not something the firmware auto-detects or negotiates — worth a one-line callout in the plan/README as a deployment checklist item, no runtime logic.

## 4. Risk handling

**Risk 1 — OTA compatibility spike. Resolved by dropping E22 entirely,
rather than by solving it.** This risk originally concerned reverse-
engineering the EBYTE E22's proprietary over-the-air framing (the old
`sx126x/src/uart.rs::send()` had a telling comment: "the E22 strips the
routing header on TX; the receiver sees `[src_high, src_low, data...]`" —
its addressing behavior was worked out empirically, never from published
documentation). Rather than resolve that spike, the RPi side switched
entirely to the bare RFM95W/SX1276-over-SPI setup that was originally
proposed here only as a fallback — the `sx126x`/E22 driver has been
removed from the codebase. Both ends now run identical hardware and
identical code (`sx127x`) — compatibility isn't a risk to spike on, it's
true by construction.

**Risk 2 — USB-host reconnection.** SI masters get unplugged/replugged in the field. The ESP32 firmware mirrors the hotplug pattern already in `sportident.rs` (there: a udev-event background thread; on ESP32: ESP-IDF `usb_host` client connect/disconnect callbacks feeding the same kind of channel-based worker restart loop) — same shape, different transport.

**Risk 3 — Address collisions.** Covered under Addressing above — a deployment-checklist concern, not a code one.

## 5. Testing

- `sportident-proto`: pure unit tests for CRC16/framing/`parse_si9`, ported over during extraction — no hardware needed, matches the existing testing style already in this repo.
- `sx127x`: pure register-encoding unit tests (already in place, since `lora-server` depends on this crate today). Actual on-air interoperability between the RPi and an ESP32 node can only be verified with two real radios once the firmware exists.
- RPi-side `PUNCH`-payload parsing (`backend.rs`/`ui.rs`): extracted into a small testable function, unit tested the same way.
- ESP32 firmware, Wi-Fi config page, USB-host+SI-master integration, OLED display: no automated tests — verified on real hardware, consistent with this project's existing "CLI/TUI has no automated tests, verified by running on real hardware" precedent.
