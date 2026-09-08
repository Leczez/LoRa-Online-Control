//! Field-editable node settings (address/dest/frequency), persisted to NVS so
//! they survive reboots. Defaults match lora-base-station's actual
//! deployment (see esp32-node/src/main.rs) until a technician changes them
//! via the Wi-Fi config page.

use esp_idf_svc::nvs::{EspNvs, NvsDefault};

const NAMESPACE: &str = "nodecfg";
const KEY_ADDR: &str = "addr";
const KEY_DEST: &str = "dest";
const KEY_FREQ: &str = "freq";
const KEY_NETWORK_ID: &str = "netid";

/// Not `Copy` (network_id is a String) — callers that need it in more than
/// one place (main.rs handing it to both wifi_config::run and the radio
/// loop, wifi_config.rs's two HTTP handler closures) clone explicitly.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub addr: u16,
    pub dest: u16,
    pub freq_hz: u32,
    /// Shared deployment identifier — must match lora-server's
    /// --network-id. See docs/protocols/lora_online_control_protocol.md,
    /// "Network Identification".
    pub network_id: String,
}

impl NodeConfig {
    pub fn load(nvs: &EspNvs<NvsDefault>, defaults: NodeConfig) -> Self {
        let mut netid_buf = [0u8; 32];
        let network_id = nvs
            .get_str(KEY_NETWORK_ID, &mut netid_buf)
            .ok()
            .flatten()
            .map(|s| s.to_string())
            .unwrap_or(defaults.network_id);

        Self {
            addr: nvs.get_u16(KEY_ADDR).ok().flatten().unwrap_or(defaults.addr),
            dest: nvs.get_u16(KEY_DEST).ok().flatten().unwrap_or(defaults.dest),
            freq_hz: nvs.get_u32(KEY_FREQ).ok().flatten().unwrap_or(defaults.freq_hz),
            network_id,
        }
    }

    pub fn save(&self, nvs: &mut EspNvs<NvsDefault>) -> anyhow::Result<()> {
        nvs.set_u16(KEY_ADDR, self.addr)?;
        nvs.set_u16(KEY_DEST, self.dest)?;
        nvs.set_u32(KEY_FREQ, self.freq_hz)?;
        nvs.set_str(KEY_NETWORK_ID, &self.network_id)?;
        Ok(())
    }
}

pub fn open_nvs() -> anyhow::Result<EspNvs<NvsDefault>> {
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    let partition = EspDefaultNvsPartition::take()?;
    Ok(EspNvs::new(partition, NAMESPACE, true)?)
}
