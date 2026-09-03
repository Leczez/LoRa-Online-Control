## LoRa Online Control Protocol

The purpose of this protocol is to send the SPORTident punches from a SPORTident device to MEOS using LoRa.


## Protocol Structure
- Packets should have some form of crc or identifier to make sure that the data received is from one of our own nodes and the data is correct.
- Heartbeat data should be sent to make sure that the receiver knows that the node is alive, together with the voltage of the battery of the node if it has one and other status data that could be necessary.

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

- **Origin address travels in the payload, not just the radio header.**
  The radio-layer address on each hop is only the *next hop*, so after one
  or more relay hops the base station's `pkt.src_addr` is the last relay,
  not the punching node. Punch and heartbeat payloads carry the originating
  node's address explicitly, so the base station always attributes a punch
  to the right control point regardless of how many hops it took — this
  also means the same parsing path handles direct and relayed traffic
  identically, with no special-casing.
- **A small hop-count/TTL field is cheap insurance.** The topology is fixed
  by configuration so loops shouldn't happen, but a 1-byte hop count
  decremented per relay hop (packet dropped at zero) is nearly free and
  guards against a future misconfiguration silently spamming the channel.
- **Command packets route the same way, reversed.** A command destined for
  a node beyond direct range travels base → relay → target using the same
  fixed path, just the other direction — no separate mechanism needed.
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
16ms (true for SF11+ at 125kHz) — both `sx126x` and `sx127x` compute this
automatically from the configured SF/BW rather than needing it set by hand.
