# Base Station Rack — Design

**Date:** 2026-09-03

## 1. Goals & scope

**Goal:** Build a portable base station in a 10" mini rack, housing everything
the event needs in one physical unit: a small display, a Dell OptiPlex Micro,
the Raspberry Pi LoRa base station, a network switch, and power.

**In scope:**
- 10" rack sizing (U budget) and a mounting plan for gear that isn't natively
  rack-mount (OptiPlex Micro, Pi, display).
- Network topology: switch as the LAN hub, venue-provided WAN uplink.
- Power: mains-in as primary, with a UPS/battery buffer sized against the
  actual load — per your input, planned for both "mains available" and
  "fully off-grid" without a redesign if the answer changes later.
- Software role assignment across the OptiPlex and the Pi.

**Out of scope:**
- Weatherproofing/ruggedization beyond basic transport — assumed indoors or
  under cover at the venue, not directly rained on.
- MEOS's own event configuration — this plan only covers where it runs and
  how `roc-server`/`lora-server` feed it, not MEOS setup itself.
- The ESP32 field node hardware — covered separately in
  `docs/superpowers/specs/2026-08-25-esp32-si-punch-node-design.md`.

## 2. Rack format & U budget

10" racks use the same 1.75" U height as 19" racks, just a narrower
(~250mm) mounting width — the format sold as small "network cabinets" for
home-lab/patch-panel use, typically 6U–9U, open-frame or a shallow enclosed
cabinet.

| Item | Rack space | Notes |
|---|---|---|
| Small display | 2U | No native 10" rack-mount monitor arm exists at this width — plan on a shelf + VESA bracket, or Velcro-mounting the display's own stand |
| Dell OptiPlex Micro | 1U (shelf) | ~7"×7"×1.4" footprint sits comfortably on one vented shelf |
| Raspberry Pi (+ LoRa module) | shares a shelf, or its own 1U tray | Keep the SPI/GPIO wiring to the RFM95W/SX1276 accessible — see §3 |
| Network switch | shares the Pi's shelf | Reusing an existing metal-cased Netgear switch, estimated ~15cm (~5.9") wide — comfortable margin against the rack's ~25.4cm (10") internal width, confirmed to fit alongside the Pi |
| Power strip / PDU | 1U | Basic outlet strip is fine; a proper rack PDU with fusing if committing to rack-native gear throughout |

Total: roughly **5U** of actively-mounted gear (display 2U, OptiPlex 1U
shelf, Pi+switch shared 1U shelf, PDU 1U). **Decision: 6U rack**, since this
is confirmed portable — a 9U's extra cable/airflow headroom isn't worth the
added size and weight for something that gets packed up and carried between
events. With the switch confirmed to share the Pi's shelf, a 6U rack leaves
a spare U for cable slack rather than being a flat, no-margin fit.

Keep an eye on component selection going forward (display bracket depth,
shelf choice) so nothing creeps past what's budgeted here — there's much
less slack to absorb a surprise than a 9U would have had.

The UPS/battery (§5) is not counted in the table — it's unlikely to fit
rack-mounted at this width and is better placed at the base of, or beside,
the rack (which also keeps it out of the U budget entirely, portable or
not).

## 3. Mounting non-rack-native gear

None of the OptiPlex Micro, the Pi, or most small displays are natively
rack-mount. Plan:

- Universal 1U vented rack shelves (cheap, work in any 10"/19" rack) carry
  the OptiPlex and the Pi.
- OptiPlex Micro has an official VESA mount plate as an accessory — use it
  to fix the unit to the shelf (or the rack's rear upright) rather than
  relying on friction/Velcro alone.
- Pi: a basic case with mounting holes, screwed to the shelf. Leave slack
  in the routing for the LoRa module's SPI/GPIO wiring and, critically,
  **route the antenna cable outside the rack** (to the rack's exterior or
  a nearby window) — an enclosed metal rack is exactly the kind of RF
  shielding that would undo all the range work already put into the SF/BW
  choice.
- **Antenna panel mount:** the RFM95W/SX1276's on-board u.FL connector isn't
  meant for repeated mating or panel use, so route it out via a single
  integrated **u.FL-to-SMA-female bulkhead pigtail** (u.FL end plugs into
  the module; the SMA-female end *is* the panel-mount bulkhead — one part,
  not a separate adapter plus a separate cable), through a drilled hole in
  a flat area of the case top clear of the rails/lid. The antenna's own
  connector then screws directly onto that external SMA-female thread.
  **Confirm the antenna is standard SMA and not RP-SMA** (reverse-polarity,
  common on WiFi gear, looks identical but won't mate) before ordering.
- **If the antenna needs to sit further away** (e.g. mast-mounted for
  height/line-of-sight over terrain — usually worth the tradeoff below),
  don't extend with the same thin coax the pigtail uses: cheap RG174-class
  cable loses ~0.5–0.8dB/m at 433MHz, so 5m would cost 2.5–4dB of the link
  budget the SF11/12 choice was tuned to maximize. Use **LMR-400** instead
  (~0.13dB/m at 433MHz, so ~0.7dB over 5m) or **LMR-240** as a thinner/
  cheaper middle ground (~0.35dB/m, ~1.7dB over 5m), run as a separate
  cable from the bulkhead to the antenna. Gender note: this cable needs to
  be **SMA-male (into the bulkhead) to SMA-female (accepting the antenna's
  plug)**, not male-to-male.
- **Since this rack is portable and gets repeatedly packed/transported**,
  fasten everything with screws rather than Velcro/friction alone — the
  OptiPlex's VESA plate and the Pi's case mounting holes both support this
  already, so it costs nothing beyond using the mounting points that exist.
  Vibration during transit is what turns a friction-mounted device into a
  loose one after a few trips, not normal use at the venue.
- Favor an **enclosed rack case with a lid** over an open-frame rack, given
  the transport pattern — it protects the gear (and the cabling) during
  loading/unloading and the drive between events, which an open frame does
  nothing for.

## 4. Networking

- The switch is the LAN hub: OptiPlex, Pi, and the display (if it's a
  networked model) all connect to it.
- **WAN uplink:** venue provides internet, per your confirmation — the
  switch's uplink port goes to the venue's ethernet drop, or a WiFi bridge
  if the venue only offers WiFi.
- **`roc-server` reachability:** recommend running it on the OptiPlex
  alongside MEOS (see §6) — MEOS then talks to it over localhost, so the
  critical MIP/ROC path has zero dependency on the LAN or the venue's
  uplink even if either hiccups.
- **`lora-server` (Pi) → `roc-server`:** a same-subnet hop through the
  switch, not touching the venue's uplink at all — punch delivery from the
  radio side stays fully local to the rack regardless of internet state.
- The display, if it's just showing `lora-tui`, doesn't need networking at
  all (direct HDMI from the Pi). Flag if you want it to show something else
  instead — that's a separate requirement from what's assumed here.

## 5. Power

Planned to work whether mains ends up being the primary supply or just a
backup buffer, per your "plan for both" answer:

- **Primary:** a power strip (or rack PDU) fed from one mains cord to the
  venue's outlet.
- **Backup/buffer — sizing:**

  | Component | Typical draw |
  |---|---|
  | Dell OptiPlex Micro | ~30–65W (check the specific model's PSU rating — commonly a 65W or 90W external brick) |
  | Raspberry Pi + LoRa module | ~5–10W |
  | Small display | ~10–25W (size/backlight dependent) |
  | Network switch | ~5–10W (existing Netgear switch, unmanaged — nothing in this design needs PoE) |
  | **Total** | **~60–110W continuous** |

  A mid-size UPS (600VA/~360W class) gives multi-hour ride-through margin
  against this load for a mains blip — comfortable rather than tight.

- **If off-grid ever becomes the actual primary supply** (not just a
  buffer), the same UPS/battery becomes the load-bearing power source
  instead — but then it needs to be sized against the *full event
  duration*, not a "ride through a blip" runtime, which is a materially
  different (and larger) battery. Worth pinning down once you know which
  scenario is real for a given venue.

## 6. Software role assignment

- **Dell OptiPlex Micro** — runs MEOS itself, plus `roc-server` colocated
  on the same machine (the Docker image already built for it runs fine
  regardless of the OptiPlex's OS). MEOS reaches its punch feed over
  localhost.
- **Raspberry Pi** — runs `lora-server` only, matching its existing role:
  the radio-facing daemon, pushing punches to `roc-server` over the LAN.
- **Display** — recommend `lora-tui` attached to the Pi's `lora-server`:
  the single most useful "is everything actually working" signal for
  someone glancing at the rack mid-event (link status, RX traffic, punch
  flow).

## 7. Open questions

- Exact display model/size — determines the mounting hardware and whether
  2U is enough.
- OptiPlex Micro's actual PSU wattage (model-specific) — needed to finalize
  UPS sizing precisely rather than the range in §5.
- Whether "off-grid" is ever the *real* primary scenario for a venue this
  rack goes to, since that reshapes the power section's sizing math (see
  §5's last point).
