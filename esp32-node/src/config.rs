//! Field-editable node settings (address/dest/frequency), persisted to NVS so
//! they survive reboots. Defaults match lora-3b-2's actual deployment (see
//! esp32-node/src/main.rs) until a technician changes them via the Wi-Fi
//! config page.

use esp_idf_svc::nvs::{EspNvs, NvsDefault};

const NAMESPACE: &str = "nodecfg";
const KEY_ADDR: &str = "addr";
const KEY_DEST: &str = "dest";
const KEY_FREQ: &str = "freq";

#[derive(Debug, Clone, Copy)]
pub struct NodeConfig {
    pub addr: u16,
    pub dest: u16,
    pub freq_hz: u32,
}

impl NodeConfig {
    pub fn load(nvs: &EspNvs<NvsDefault>, defaults: NodeConfig) -> Self {
        Self {
            addr: nvs.get_u16(KEY_ADDR).ok().flatten().unwrap_or(defaults.addr),
            dest: nvs.get_u16(KEY_DEST).ok().flatten().unwrap_or(defaults.dest),
            freq_hz: nvs.get_u32(KEY_FREQ).ok().flatten().unwrap_or(defaults.freq_hz),
        }
    }

    pub fn save(&self, nvs: &mut EspNvs<NvsDefault>) -> anyhow::Result<()> {
        nvs.set_u16(KEY_ADDR, self.addr)?;
        nvs.set_u16(KEY_DEST, self.dest)?;
        nvs.set_u32(KEY_FREQ, self.freq_hz)?;
        Ok(())
    }
}

pub fn open_nvs() -> anyhow::Result<EspNvs<NvsDefault>> {
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    let partition = EspDefaultNvsPartition::take()?;
    Ok(EspNvs::new(partition, NAMESPACE, true)?)
}
