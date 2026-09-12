## LoRa Online Control Protocol

The purpose of this protocol is to send the SPORTident punches from a SPORTident device to MEOS using LoRa.


## Protocol Structure
- ~~Packets should have some form of crc or identifier to make sure that the data received is from one of our own nodes and the data is correct.~~ Identifier: done, see "Network Identification" below — now the radio's own sync word, not an application-layer prefix. CRC: LoRa's own hardware CRC (`crc_on` in `sx127x::Config`, on by default on both ends) already covers over-the-air corruption at the driver level — `receive()` drops a CRC-failed packet before the application ever sees it, so no separate payload-level CRC was needed on top.
- ~~Heartbeat data should be sent to make sure that the receiver knows that the node is alive, together with the voltage of the battery of the node if it has one and other status data that could be necessary.~~ Done for battery voltage/percentage, see "Heartbeats" below. Other status data (e.g. SI-master-connected state) isn't carried yet — a natural extension of the same `HB` format if needed later.

## Network Identification

Previously implemented as a plaintext deployment-ID prefix sent on *every*
frame (`--network-id`/`NetworkFilteredRadio`) — removed as part of a wire
protocol efficiency pass, since it cost airtime on every single packet for
something the radio's own hardware can already do for free: the LoRa
**sync word** (`--sync-word`/`LORA_SYNC_WORD` on lora-server,
`NodeConfig::sync_word` on esp32-node — see "RF Parameters" below for how
that gets set/verified) is checked by the chip itself during preamble
detection, rejecting a
mismatched packet *before it's even demodulated*, rather than after
receiving and parsing a whole extra prefix.

This is still a **plain-text-equivalent guard against accidental
cross-talk**, not a security mechanism — it protects against another event
running this same open-source firmware nearby, not a deliberate spoofer who
has the source and could easily match it. Every node and base station in
one deployment must use the same sync word value; **change it away from the
default (18/0x12) for a real deployment** — that specific value is a very
commonly used default across LoRa projects generally, not unique to this
one, so leaving it unchanged gives little real protection against a nearby
deployment of *this* firmware (which also defaults to it) let alone other
LoRa gear.

## Addressing

The LoRa node address and the SPORTident control/station number are two
separate things, not one:

- **LoRa node address** — a small integer, unique per physical device,
  assigned at commissioning (or derived from the device's hardware chip ID).
  Used for addressing on the radio link itself: which node a command packet
  targets, which node's traffic the base station is looking at.
- **SI station/control number** — the control code the SI unit is
  programmed with. Carried as payload data (punches already encode which
  station a punch belongs to; heartbeats should too), never as the LoRa
  address.

This split matters for a real deployment pattern: some controls run two
independent SI masters programmed with the *same* control number for
redundancy (e.g. a remote or high-traffic control). If the LoRa address
were tied to the station number, both nodes would collide on one address
and the base station could never single one out for a command. With the
split, both nodes get distinct LoRa addresses and both report the same
station number in their payloads — the base station sees "node 5 → control
31 (primary)" and "node 6 → control 31 (backup)" as clearly separate.

Including each node's hardware chip ID in heartbeats and a commissioning/
join packet (rather than on every packet, to save airtime) lets the base
station detect a genuinely dangerous failure mode: two nodes accidentally
sharing a LoRa address, which would otherwise be silently ambiguous.

## Punch Delivery

Every punch a SportIdent master hands a node is buffered to disk immediately,
unconditionally — independent of whether or when it can actually be
transmitted. Delivery over the radio link itself works as **stop-and-wait**:

- A node holds at most **one punch outstanding** at a time. It doesn't
  attempt the next buffered punch until the current one is acknowledged.
- **LoRa is a broadcast medium — every node in radio range decodes every
  packet, regardless of what `dest` its sender used.** Nothing at the radio
  layer filters by destination (see `sx127x::Sx127xSpi::send`, whose own
  `dest` argument is unused — purely the caller's bookkeeping). So `PUNCH`
  carries its intended final destination explicitly in the payload itself —
  `PUNCH <origin> <dest> <card_id> <station>:<time_s>,...` — and every node
  that overhears it checks `dest == own_addr` before reacting: only the
  addressed node buffers and acks it. A non-addressed, non-relay node still
  logs it (full visibility into everything overheard — e.g. `lora-tui`'s
  packet log or the web dashboard), it just doesn't buffer or ack. A relay
  forwards the raw payload unchanged, so `dest` stays the true final
  destination through every hop rather than needing to be rewritten
  per-hop. (`Frame::Command`/`Frame::Ack`/`Frame::PunchAck` already carried
  an explicit `target`/`node` field checked the same way — `PUNCH` was the
  one payload type missing this until now.)
- The receiver (whoever gets a `PUNCH` payload) buffers it on its own end,
  then sends back a `PACK` frame naming the sending node and the card
  involved — e.g. `PACK <node> <card_id>` — so that on a shared channel with
  multiple nodes, only the node actually waiting for that ack acts on it.
- If no ack arrives within a retry window, the node resends the exact same
  payload and keeps waiting — **there's no give-up count for punches**,
  unlike the bounded retry used for command packets below. A punch is real
  event data, not a settings tweak; it's held and retried indefinitely
  rather than ever silently dropped.
- The ack itself is sent best-effort, not separately retried — if it's lost,
  the sender's own retry timeout resends the punch, which prompts another
  ack attempt. This is self-healing without needing the ack path itself to
  be reliable.
- Since only one card_id is ever outstanding per node at a time, the ack
  only needs to name it — no separate sequence number was needed on top of
  what `PUNCH` already carries.

## lora-server ↔ roc-server reachability

Separate from the LoRa-radio heartbeat below: `lora-server` and `roc-server`
each expose a plain `GET /health` over HTTP, and each can optionally run a
background thread that polls the *other's* `/health` on an interval
(`--roc-health-url` on lora-server, `--lora-health-url` on roc-server; both
default-off, and logging only on state transitions so a healthy link
doesn't spam the log). This checks the network path between the two
servers, independent of whether punches are actually flowing — a
`lora-server` with nothing to push would otherwise give no signal at all
that its link to `roc-server` is down until a punch actually needed to go
out.

On lora-server, `/health` starts serving *before* the radio hardware is
even opened, not after — and reports `200 ok` only once the SX1276/RFM95W
module has actually responded, `503 lora module not found` while still
retrying. Starting it later (after a successful radio connect, as this
originally shipped) meant a daemon stuck retrying "SPI module not
responding" forever looked identical from outside to the process not
running at all — `/health` simply wasn't listening yet during that window.

If both processes run on the same host (e.g. the planned Pi 5
consolidation) and `roc-server` runs in Docker, see the `extra_hosts` note
in `roc-server/docker-compose.yml` — a container's own `localhost` won't
reach a native process on the same machine.

## lora-server web API (dashboard + lora-tui transport)

`lora-server` embeds an HTTP server (`--web-listen`, default
`0.0.0.0:8082`) that is both a browser-facing status dashboard and
`lora-tui`'s *only* transport when attaching to a running daemon — there is
no other control channel; `lora-tui` is a plain HTTP client, so it can
attach to a `lora-server` on a different machine, not just one on the same
host.

- `GET /status.json` — this daemon's own `own_addr`, radio/roc-server
  reachability, the per-node health table, and the recent packet log.
  `own_addr` is what `lora-tui` displays as its own address on attach —
  discovered from the daemon rather than taken from a `--addr` flag of its
  own (which it doesn't have), since a hardcoded client-side default has no
  way to stay correct across different daemons you might point it at. Each
  log line carries a monotonically increasing `seq` (never reused, even
  once the line itself ages out of the bounded history) so a polling client
  can tell "new since last poll" apart from "same line, just formatted with
  a fresher age" — comparing the line text alone can't do that. `lora-tui`'s
  `HttpRadio` (`backend.rs`) polls this every 500ms, tracks the highest
  `seq` it's seen, and feeds any newer lines through
  `parse_rx_line`/`parse_status_line` — reusing 100% of the existing
  packet/heartbeat/command-ack display logic.
- `GET /` — the same data as an HTML page.
- `POST /testpunch` (form: `card_id`, `station`, `time_s`) — injects a
  synthetic punch tagged `source="test"` through the real send/retry/ack
  pipeline.
- `POST /send` (raw text body, not form-encoded — a radio payload can
  contain `&`/`=`) — sends to the daemon's currently configured `dest`.
- `POST /setdest` (form: `dest`) — changes the daemon's `dest`.
- `POST /cmd` (form: `target`, `heartbeat_interval_secs`) — originates a
  Command Packet (see below) toward `target`, tracked with the same
  retry/ack bookkeeping any command gets.
- `POST /clearpunch` (form: `id`) — permanently abandons one unsent local
  punch (a genuine delete, not mark-sent, since it was never actually
  delivered). If that punch happens to be the one currently in flight (mid
  retry), the daemon also drops its in-memory retry state, not just the DB
  row — otherwise it would keep retrying a payload whose backing row no
  longer exists, forever, since nothing else would ever clear it.
- `POST /clearpunches` (no body) — the same, but for every unsent local
  punch at once.

All six POST endpoints forward to the daemon's internal command channel,
read by the same `run_daemon_loop` dispatch regardless of which endpoint a
command came in on — a browser form and `lora-tui` are two callers of the
same underlying commands.

## Heartbeats

Uplink, `HB` — plain and untracked, no ack, no retry, just a liveness
signal. `parse_heartbeat` in `backend.rs` accepts three shapes:

- Bare `HB` — no battery, no SI-master status. What `lora-server` itself
  sends (a relay has neither concept), and older firmware.
- `HB <battery_pct> <battery_mv>`, e.g. `HB 82 3950` — legacy battery-only
  shape, kept for backward compatibility though nothing currently emits it.
- `HB <battery_pct-or-"-"> <battery_mv-or-"-"> <0-or-1>` — what ESP32 punch
  nodes actually send (`esp32-node/src/main.rs`), e.g. `HB 82 3950 1` or
  `HB - - 0` if the battery read failed. The trailing field is whether an SI
  master is currently connected to that node, reported independently of
  battery data — `-` `-` (not omitting the fields) keeps the count fixed at
  three so the parser doesn't have to guess which fields are missing.

Heartbeats and SI-master connectivity are sent from the node's radio/main
thread, entirely independent of whether an SI master is actually plugged in
(SI reading runs on its own thread — see `spawn_si_reader_thread` in
`esp32-node/src/main.rs`): a node with a dead or unplugged reader but a
healthy radio still shows up as alive, just flagged as having no reader,
rather than going silent and looking indistinguishable from a dead node.

**Heartbeats don't carry an origin field, so they can't relay** (same
limitation as noted under Relay Nodes below).

## Command Packets

The base station needs a way to change limited settings on a deployed node
(heartbeat interval, TX power) without physically hiking back out to it.
This is a new **downlink** direction — everything else in this protocol is
uplink (node → base station) — addressed to one node's LoRa address at a
time, matching the addressing scheme above.

- **Half-duplex constraint.** These radios can't transmit and receive
  simultaneously, so a node can't be commanded mid-transmit. A node opens a
  short listen window after each uplink send (heartbeat or punch) to check
  for a pending command before returning to its normal receive/sleep cycle.
- **Acknowledge before applying.** The base station doesn't consider a
  command delivered until the node acks it; LoRa drops packets, so a
  command with no ack gets retried, not assumed to have landed.
- **Scope stays narrow.** Only settings that can't strand the node are
  remotely changeable — heartbeat interval, TX power. Anything that
  affects the radio link itself (spreading factor, bandwidth, frequency)
  is out of scope for remote command: a bad change can leave a node unable
  to ever hear the "undo" instruction, and it can only be fixed by physical
  access again. Those settings stay commissioning-time only.

## Version Reporting

Every node and lora-server instance embeds its own build version
(`<semver>+<git-sha>[.dirty]` — see each crate's `version.rs`/`VERSION`
const, and the repo-root `VERSION` file for the semver half). Two ways it
reaches the base station:

- **Boot announcement** — `VERSION <origin> <version>`, unprompted, right
  after the radio comes up. Passive discovery: an operator doesn't have to
  remember to query every node after a redeploy, it just shows up in the
  node table the next time each node reboots. Re-sent every
  `CONFIG_VERIFY_RETRY_INTERVAL` for up to `CONFIG_VERIFY_WINDOW` after
  boot (not just once) — see "RF Parameters"' "Config verification" below,
  since this doubles as that mechanism's own probe.
- **On-demand query** — `VQUERY <target>` (downlink), answered with the
  same `VERSION <origin> <version>` uplink frame. Reachable via
  `lora-tui`'s `/queryversion <target-addr>`, the web dashboard's "Query
  node version" form, or `POST /queryversion` (form field `target`)
  directly.

Both are deliberately simple compared to `Command`/`Ack`:

- **No `commander`/relay-routing field.** Same accepted limitation as `HB`
  (see above) — a `VersionReport` is recorded from whoever overheard it,
  unconditionally, the same way battery/SI-master state already is from any
  overheard `HB`. This is a one-off diagnostic value, not safety-critical
  config, so the complexity of `Command`'s retry-tracked, relay-aware
  exchange isn't worth it here.
- **`lora-server` acks every `VersionReport` it receives with a
  `ConfigAck`** (see "RF Parameters" below), whether that report was a
  `VQUERY` reply or the unprompted boot announcement — it doesn't
  distinguish which prompted it, since either is equally valid proof the
  reporting node's current LoRa mode is reaching the base station. This
  `ConfigAck` isn't itself retried or relayed, and a `VQUERY` reply that
  goes unheard just means re-issuing the query or waiting for the node's
  next boot — only the boot-announcement path (see below) actually retries
  on a missing ack, and only for the 60s config-verification window, not
  indefinitely.
- **`VersionQuery` still relays like `Command`** (`--relay`, forwarding
  toward `target` if not addressed to this node) so a query can still reach
  a node behind a relay — only the *reply* (and `ConfigAck`) skip relay/
  commander routing, same as `HB`.

## Relay Nodes

A control point out of direct range of the base station can reach it via a
relay — another node (either a dedicated relay-only device, or a normal
control-point node doing double duty) that forwards its traffic on. The
topology for an event is known at setup time, so relay paths are **fixed
and configured at commissioning**, not discovered dynamically — no route
discovery, no loop-avoidance protocol, just "node 7 relays via node 5."
This keeps a relay node's logic small: it either has traffic of its own to
send, or a packet arrives that isn't addressed to it but matches its
configured forwarding rule, and it re-transmits that packet on its own next
uplink turn.

- **Origin/commander address travels in the payload, not just the radio
  header — implemented for punches and commands.** The radio-layer address
  on each hop is only the *next hop*, so after one or more relay hops
  `pkt.src_addr` is the last relay, not the original node. `PUNCH <origin>
  <dest> <card_id> ...` carries the punching node's address explicitly
  (`CardReadout::to_payload`/`parse_payload` in `sportident.rs`), and
  `Command`/`Ack` carry a `commander` field the same way, so the same
  parsing path handles direct and relayed traffic identically, with no
  special-casing, and both `PunchAck` and a command's `Ack` can be relayed
  back toward whoever they're ultimately for. **Heartbeats don't carry an
  origin field yet** — only punch and command traffic relays for now.
- **A relay is enabled with `--relay`; forwarding is best-effort, not
  itself retried at the relay hop.** A relay node re-transmits a punch/
  command (or an ack addressed elsewhere) unchanged toward its own
  `--dest` (uplink) or toward the named target/commander directly
  (downlink), without tracking or retrying that specific hop. Reliability
  still comes from the end-to-end stop-and-wait between the original
  sender and the final consumer (punches) or the bounded retry the
  original commander already runs (commands) — a dropped relay hop just
  means that existing retry fires again, and gets relayed again.
- **Commanding a node other than your current `--dest` now needs a
  `SET_DEST` first.** Command frames route to `--dest` (this node's own
  next hop) rather than straight to the named target, exactly like uplink
  punch traffic already does — that's what makes relaying possible, and it
  makes `CMD` consistent with the existing `POST /send` behavior instead of
  being the one exception that assumed direct reach.
- **Hop-count/TTL is not implemented yet.** Still worth adding as cheap
  insurance against a future misconfigured relay loop, but the current
  fixed, hand-configured topology doesn't need it to function correctly
  today — tracked as a follow-up, not a blocker.
- **Dual-role nodes share one airtime/duty-cycle budget.** A node that is
  both a control point and a relay is carrying its own traffic plus
  whatever it forwards on the same radio, same duty-cycle allowance, same
  battery. That's the real cost of combining the roles — worth planning
  power budget for specifically, rather than assuming a relay node draws
  the same as a plain control-point node.

## RF Parameters

To maximize range, we use the lowest practical bitrate rather than the
fastest. LoRa's spreading factor (SF) and bandwidth (BW) both trade bitrate
for receiver sensitivity — every SF step roughly doubles symbol length and
meaningfully improves sensitivity; halving the bandwidth does the same.

Applied as the default: **SF11 at 125kHz** (433MHz, CR 4:5) —
`LORA_SF`/`SPREADING_FACTOR` on `lora-server` and `esp32-node`
respectively, both defaulting to 11. Not the absolute extreme (SF12 at
7.8kHz, or even SF12 at 125kHz), because:

- **Airtime cost.** Symbol time scales as `2^SF / BW`, so the narrowest
  settings push airtime into multiple seconds per message. That eats into
  the ETSI 433MHz duty-cycle allowance fast, especially with a heartbeat
  every 60s across several field nodes.
- **Crystal drift margin.** These are budget SX1276/RFM9x-class modules,
  not precision-oscillator hardware. Narrower bandwidth leaves less margin
  before frequency drift pushes a signal outside the receiver's filter and
  the packet is simply missed.
- **Diminishing real-world returns.** In forest/hilly terrain, terrain
  attenuation dominates actual range far more than the last few dB of link
  budget — SF7→SF10 buys real range; SF12/7.8kHz buys comparatively little
  further while costing airtime and drift margin.

`LowDataRateOptimize` must be enabled whenever the symbol period exceeds
16ms (true for SF11+ at 125kHz) — `sx127x` computes this automatically from
the configured SF/BW rather than needing it set by hand.

**Airtime, concretely:** a ~30-byte punch frame that took roughly 70ms at
SF7 takes roughly 900ms at SF11 — about a 12x increase. A 60s heartbeat
interval still keeps that comfortably under 2% duty cycle per node, but
this is the number to revisit before shortening `LORA_HEARTBEAT_INTERVAL`
or adding more per-node periodic traffic.

**Changing SF/BW/CR on an already-commissioned node requires physical
access, but not a reflash.** Like sync word and frequency (see "Command
Packets" above), these stay out of scope for remote LoRa command changes —
a bad value could leave a node unable to ever hear the "undo" instruction.
`esp32-node` used to expose `NodeConfig::sf`/`bw_hz`/`cr` (alongside addr/
dest/freq/sync_word) on a boot-time Wi-Fi config portal for this
(`config.rs`/`wifi_config.rs`) — that portal still exists and still works,
but is **disabled by default** now (`wifi-config-portal` Cargo feature) in
favor of the self-healing mechanism below, which needs no operator present
and doesn't cost every boot a fixed 2-minute Wi-Fi window. On `lora-server`,
changing these is unchanged: edit `/etc/lora-server/env`'s
`LORA_SF`/`LORA_BW_HZ`/`LORA_CR` and restart the service — no rebuild
needed. A mismatch between the two ends still means silent
non-communication, no error on either side, so change both together.

**Config verification: a node auto-heals a bad LoRa mode instead of relying
on someone noticing.** For `CONFIG_VERIFY_WINDOW` (60s) after boot, a node
re-sends its boot announcement (`VERSION`, see "Version Reporting" above)
every `CONFIG_VERIFY_RETRY_INTERVAL` (10s) — several attempts, not a single
shot, so one lost packet in either direction can't by itself cause a false
"this config doesn't work" conclusion. `lora-server` replies to *every*
`VersionReport` it receives (whether unprompted or a reply to its own
`VQUERY`) with a `ConfigAck` frame — the node's only proof its current mode
(freq/sf/bw_hz/cr/sync_word) is actually reaching the base station. If no
`ConfigAck` arrives before the window closes, the node reverts those fields
to `NodeConfig::standard_rf()`'s known-good values, saves that to NVS, and
reconfigures the radio — **`addr`/`dest` are untouched**, since those are
per-node identity/topology assigned at commissioning, not part of "the LoRa
mode" this recovers from. The revert is persisted, so a bad config costs
exactly one 60-second boot before self-correcting permanently — the node
doesn't repeat the wait-then-revert cycle on every subsequent boot. Without
the Wi-Fi portal enabled, there is currently no *live* way to move a node
onto a non-default LoRa mode at all short of changing `default_config()`'s
consts and reflashing — its eventual replacement (a LoRa-triggered settings
mode) isn't built yet.

**Available values, for the portal/env file:**

| Axis | Values | Notes |
|---|---|---|
| Spreading factor | 7, 8, 9, 10, 11, 12 | Higher = more range, longer airtime. 11 is the current default. |
| Bandwidth | 7.8, 10.4, 15.6, 20.8, 31.25, 41.7, 62.5, 125, 250, 500 (kHz) | Lower = more range, longer airtime. Below ~20.8kHz risks exceeding this hardware's crystal drift margin (non-TCXO SX1276/RFM9x modules) — packets can simply stop arriving as temperature shifts. 125kHz is the current default; 62.5kHz is the practical next step down if more range is needed, with comfortable drift margin still intact. |
| Coding rate | 4/5, 4/6, 4/7, 4/8 | Higher denominator = more forward error correction (better tolerance of a noisy/marginal link), more airtime overhead. 4/5 is the current default. |

That's 6 × 10 × 4 = 240 raw combinations; the three axes are independent; the table above (not a full cross-product listing) is the useful reference since any SF can be paired with any BW and any CR. `sx127x::Bandwidth::from_hz`/`CodingRate::from_denominator` are the single source of truth both `lora-server` and `esp32-node` validate against, so an out-of-range value is rejected (or, on `esp32-node`'s side, silently kept at its previous value) rather than producing an unpredictable radio configuration.

*(Note: EBYTE E22 module support — a separate `sx126x` driver crate over
UART — has been dropped. Every node, base station included, now runs bare
SX1276/RFM95W modules over SPI, the same hardware and driver the ESP32
field nodes use — see the "Risk 1" section of the ESP32 node design doc
for why this consolidation happened.)*
