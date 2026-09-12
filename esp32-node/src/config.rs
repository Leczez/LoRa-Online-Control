//! Field-editable node settings (address/dest/frequency/sync word),
//! persisted to NVS so they survive reboots. Defaults match
//! lora-base-station's actual deployment (see esp32-node/src/main.rs) until
//! a technician changes them via the Wi-Fi config page.

use esp_idf_svc::nvs::{EspNvs, NvsDefault};

const NAMESPACE: &str = "nodecfg";
const KEY_ADDR: &str = "addr";
const KEY_DEST: &str = "dest";
const KEY_FREQ: &str = "freq";
const KEY_SYNC_WORD: &str = "syncword";

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
}

impl NodeConfig {
    pub fn load(nvs: &EspNvs<NvsDefault>, defaults: NodeConfig) -> Self {
        Self {
            addr: nvs.get_u16(KEY_ADDR).ok().flatten().unwrap_or(defaults.addr),
            dest: nvs.get_u16(KEY_DEST).ok().flatten().unwrap_or(defaults.dest),
            freq_hz: nvs.get_u32(KEY_FREQ).ok().flatten().unwrap_or(defaults.freq_hz),
            sync_word: nvs.get_u8(KEY_SYNC_WORD).ok().flatten().unwrap_or(defaults.sync_word),
        }
    }

    pub fn save(&self, nvs: &mut EspNvs<NvsDefault>) -> anyhow::Result<()> {
        nvs.set_u16(KEY_ADDR, self.addr)?;
        nvs.set_u16(KEY_DEST, self.dest)?;
        nvs.set_u32(KEY_FREQ, self.freq_hz)?;
        nvs.set_u8(KEY_SYNC_WORD, self.sync_word)?;
        Ok(())
    }
}

pub fn open_nvs() -> anyhow::Result<EspNvs<NvsDefault>> {
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    let partition = EspDefaultNvsPartition::take()?;
    Ok(EspNvs::new(partition, NAMESPACE, true)?)
}
