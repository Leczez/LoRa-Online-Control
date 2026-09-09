## LoRa Online Control Protocol

The purpose of this protocol is to send the SPORTident punches from a SPORTident device to MEOS using LoRa.


## Protocol Structure
- ~~Packets should have some form of crc or identifier to make sure that the data received is from one of our own nodes and the data is correct.~~ Identifier: done, see "Network Identification" below. CRC: LoRa's own hardware CRC (`crc_on` in `sx127x::Config`) already covers over-the-air corruption at the driver level — `receive()` drops a CRC-failed packet before the application ever sees it, so no separate payload-level CRC was added on top.
- ~~Heartbeat data should be sent to make sure that the receiver knows that the node is alive, together with the voltage of the battery of the node if it has one and other status data that could be necessary.~~ Done for battery voltage/percentage, see "Heartbeats" below. Other status data (e.g. SI-master-connected state) isn't carried yet — a natural extension of the same `HB` format if needed later.

## Network Identification

Every frame, of every type, is wrapped with a shared deployment ID at the
radio transport layer — transparent to `Frame::parse`/
`CardReadout::parse_payload`, which never see it. `--network-id`
(`LORA_NETWORK_ID`, default `LOC`) is prepended on send and checked on
receive (`NetworkFilteredRadio` in `backend.rs`); anything with a missing or
mismatched ID is treated as if nothing was received at all, not misparsed as
a malformed frame of our own.

This is a **plain-text guard against accidental cross-talk**, not a
security mechanism — it protects against another event running this same
open-source firmware nearby (or unrelated LoRa gear that happens to share
our sync word), not a deliberate spoofer who has the source. Every node and
base station in one deployment must be configured with the same value;
different events/deployments should use different values.

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

Recommended: **SF11 or SF12 at 125kHz** (433MHz, CR 4:5), not the absolute
extreme (SF12 at 7.8kHz), because:

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

*(Note: EBYTE E22 module support — a separate `sx126x` driver crate over
UART — has been dropped. Every node, base station included, now runs bare
SX1276/RFM95W modules over SPI, the same hardware and driver the ESP32
field nodes use — see the "Risk 1" section of the ESP32 node design doc
for why this consolidation happened.)*
