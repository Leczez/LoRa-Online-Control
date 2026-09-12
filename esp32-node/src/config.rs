//! Field-editable node settings (address/dest/frequency/sync word/LoRa
//! mode), persisted to NVS so they survive reboots. Defaults match
//! lora-base-station's actual deployment (see esp32-node/src/main.rs).
//!
//! There is currently no live way to move a node onto a *non-default* LoRa
//! mode short of changing `default_config()`'s consts and reflashing — the
//! Wi-Fi config portal that used to do this (wifi_config.rs) is disabled by
//! default (see its own doc comment) in favor of the ack-based
//! auto-revert-to-standard mechanism in main.rs, and its eventual
//! replacement (a LoRa-triggered settings mode) isn't built yet.

use esp_idf_svc::nvs::{EspNvs, NvsDefault};

const NAMESPACE: &str = "nodecfg";
const KEY_ADDR: &str = "addr";
const KEY_DEST: &str = "dest";
const KEY_FREQ: &str = "freq";
const KEY_SYNC_WORD: &str = "syncword";
const KEY_SF: &str = "sf";
const KEY_BW_HZ: &str = "bwhz";
const KEY_CR: &str = "cr";

/// The known-good LoRa mode `NodeConfig::standard_rf()` reverts to when a
/// node's current mode goes unacknowledged during its post-boot
/// verification window (see main.rs) — deliberately the same values
/// `default_config()` starts a brand new node with, so there is exactly one
/// place these can drift out of sync with each other: nowhere.
pub const STANDARD_FREQ_HZ: u32 = 433_000_000;
pub const STANDARD_SYNC_WORD: u8 = 0x12;
pub const STANDARD_SF: u8 = 11;
pub const STANDARD_BW_HZ: u32 = 125_000;
pub const STANDARD_CR: u8 = 5;

#[derive(Debug, Clone, Copy)]
pub struct NodeConfig {
    pub addr: u16,
    pub dest: u16,
    pub freq_hz: u32,
    /// Must match lora-server's --sync-word — this is the *only* guard
    /// against accidental cross-talk with another event running this same
    /// firmware nearby now (a plaintext "network ID" sent on every frame
    /// used to do this at the application layer; removed in favor of the
    /// sync word, which the radio's own hardware already checks during
    /// preamble detection, for free, before it even demodulates a
    /// mismatched packet). See docs/protocols/lora_online_control_protocol.md,
    /// "Network Identification".
    pub sync_word: u8,
    /// LoRa spreading factor (7-12) — must match lora-server's own --sf
    /// exactly, or the two ends simply never hear each other (no error on
    /// either side). Field names/units here deliberately mirror
    /// lora-server's Args (sf/bw_hz/cr) for easy cross-reference during
    /// setup. See docs/protocols/lora_online_control_protocol.md,
    /// "RF Parameters", for the range/airtime/crystal-drift tradeoffs
    /// behind each value.
    pub sf: u8,
    /// LoRa bandwidth in Hz — one of the 10 values `sx127x::Bandwidth`
    /// supports (see `Bandwidth::from_hz`); an unrecognized value falls
    /// back to 125kHz rather than failing to boot, since a bad number here
    /// (a NodeConfig saved by an older firmware, or a hand-edited value)
    /// shouldn't strand the node the way it would on lora-server (which can
    /// just refuse to start and get restarted with a fix).
    pub bw_hz: u32,
    /// LoRa coding rate denominator (5-8, meaning 4/5..4/8) — same
    /// fallback-on-invalid reasoning as `bw_hz`.
    pub cr: u8,
}

impl NodeConfig {
    pub fn load(nvs: &EspNvs<NvsDefault>, defaults: NodeConfig) -> Self {
        Self {
            addr: nvs.get_u16(KEY_ADDR).ok().flatten().unwrap_or(defaults.addr),
            dest: nvs.get_u16(KEY_DEST).ok().flatten().unwrap_or(defaults.dest),
            freq_hz: nvs.get_u32(KEY_FREQ).ok().flatten().unwrap_or(defaults.freq_hz),
            sync_word: nvs.get_u8(KEY_SYNC_WORD).ok().flatten().unwrap_or(defaults.sync_word),
            sf: nvs.get_u8(KEY_SF).ok().flatten().unwrap_or(defaults.sf),
            bw_hz: nvs.get_u32(KEY_BW_HZ).ok().flatten().unwrap_or(defaults.bw_hz),
            cr: nvs.get_u8(KEY_CR).ok().flatten().unwrap_or(defaults.cr),
        }
    }

    // Only called from wifi_config.rs's /save handler — the ack-based
    // config-verification revert in main.rs deliberately does NOT call this
    // (see its own comment for why), so with the wifi-config-portal feature
    // off this has no callers at all.
    #[cfg_attr(not(feature = "wifi-config-portal"), allow(dead_code))]
    pub fn save(&self, nvs: &mut EspNvs<NvsDefault>) -> anyhow::Result<()> {
        nvs.set_u16(KEY_ADDR, self.addr)?;
        nvs.set_u16(KEY_DEST, self.dest)?;
        nvs.set_u32(KEY_FREQ, self.freq_hz)?;
        nvs.set_u8(KEY_SYNC_WORD, self.sync_word)?;
        nvs.set_u8(KEY_SF, self.sf)?;
        nvs.set_u32(KEY_BW_HZ, self.bw_hz)?;
        nvs.set_u8(KEY_CR, self.cr)?;
        Ok(())
    }

    /// Returns this config with its LoRa mode (freq/sf/bw/cr/sync word)
    /// reset to the known-good standard values, leaving `addr`/`dest`
    /// untouched — those are per-node identity/topology, assigned at
    /// commissioning, not part of "the LoRa mode" this exists to recover
    /// from a bad value in. Used by main.rs when a node's current mode goes
    /// unacknowledged during its post-boot verification window.
    pub fn standard_rf(self) -> Self {
        Self {
            freq_hz: STANDARD_FREQ_HZ,
            sync_word: STANDARD_SYNC_WORD,
            sf: STANDARD_SF,
            bw_hz: STANDARD_BW_HZ,
            cr: STANDARD_CR,
            ..self
        }
    }
}

pub fn open_nvs() -> anyhow::Result<EspNvs<NvsDefault>> {
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    let partition = EspDefaultNvsPartition::take()?;
    Ok(EspNvs::new(partition, NAMESPACE, true)?)
}
