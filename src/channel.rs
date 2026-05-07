use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// JSON / TOML channel entry (UTF-8 `name`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    /// e.g. `ISDBT`, `DVBT` (string for serde simplicity).
    pub delivery: String,
    pub frequency: u32,
    #[serde(default)]
    pub bandwidth_hz: u32,
    pub service_id: u16,
    /// Optional PIDs from zap format (first video / audio used by some tools).
    #[serde(default)]
    pub video_pids: Vec<u16>,
    #[serde(default)]
    pub audio_pids: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelsFile {
    pub channels: Vec<Channel>,
}

impl Channel {
    /// Build from a flat `KEY = VAL` map (same keys as dvbv5 `.conf` body). `display_name` is the
    /// stable channel id / label (no `DVBR_NAME` lookup).
    pub fn from_named_tuning(display_name: &str, kv: &HashMap<String, String>) -> Result<Self, String> {
        let delivery = kv
            .get("DELIVERY_SYSTEM")
            .cloned()
            .unwrap_or_else(|| "ISDBT".to_string());
        let frequency: u32 = kv
            .get("FREQUENCY")
            .ok_or_else(|| "missing FREQUENCY".to_string())?
            .parse()
            .map_err(|e| format!("FREQUENCY: {e}"))?;
        let bandwidth_hz: u32 = kv
            .get("BANDWIDTH_HZ")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let service_id: u16 = kv
            .get("SERVICE_ID")
            .ok_or_else(|| "missing SERVICE_ID".to_string())?
            .parse()
            .map_err(|e| format!("SERVICE_ID: {e}"))?;

        let mut video_pids = Vec::new();
        if let Some(s) = kv.get("VIDEO_PID") {
            for p in s.split_whitespace() {
                if let Ok(v) = p.parse::<u16>() {
                    video_pids.push(v);
                }
            }
        }
        let mut audio_pids = Vec::new();
        if let Some(s) = kv.get("AUDIO_PID") {
            for p in s.split_whitespace() {
                if let Ok(v) = p.parse::<u16>() {
                    audio_pids.push(v);
                }
            }
        }

        Ok(Channel {
            name: display_name.to_string(),
            delivery,
            frequency,
            bandwidth_hz,
            service_id,
            video_pids,
            audio_pids,
        })
    }

    /// `section_title` is the `[...]` header text. Optional `DVBR_NAME` / `CHANNEL_LABEL`
    /// (UTF-8) override the stored [`Channel::name`] for display and CLI lookup; tuning still
    /// uses the full key/value map (extra keys are ignored by the frontend chain).
    pub fn from_dvbv5_section(section_title: &str, kv: &HashMap<String, String>) -> Result<Self, String> {
        let display_name = kv
            .get("DVBR_NAME")
            .or_else(|| kv.get("CHANNEL_LABEL"))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| section_title.to_string());

        Self::from_named_tuning(&display_name, kv)
    }
}
