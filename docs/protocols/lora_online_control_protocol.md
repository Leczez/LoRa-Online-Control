## LoRa Online Control Protocol

The purpose of this protocol is to send the SPORTident punches from a SPORTident device to MEOS using LoRa.


## Protocol Structure
- Packets should have some form of crc or identifier to make sure that the data received is from one of our own nodes and the data is correct.
- Heartbeat data should be sent to make sure that the receiver knows that the node is alive, together with the voltage of the battery of the node if it has one and other status data that could be necessary.
- The address  of the node should match the number that the SPORTident unit is programmed with.

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
