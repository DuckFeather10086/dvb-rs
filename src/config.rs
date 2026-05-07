use crate::channel::{Channel, ChannelsFile};
use crate::error::{Error, Result};
use serde_json;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Channel plus raw `[section]` key/values for full DVBv5 tuning (`dvbv5-zap` compatibility).
#[derive(Debug, Clone)]
pub struct DvbV5Entry {
    pub channel: Channel,
    pub raw: HashMap<String, String>,
}

/// Load [`ChannelsFile`] from JSON.
pub fn load_channels_json(path: &Path) -> Result<ChannelsFile> {
    let data = fs::read_to_string(path)?;
    let f: ChannelsFile = serde_json::from_str(&data).map_err(|e| Error::Parse(e.to_string()))?;
    Ok(f)
}

/// Load [`ChannelsFile`] from TOML.
pub fn load_channels_toml(path: &Path) -> Result<ChannelsFile> {
    let data = fs::read_to_string(path)?;
    let f: ChannelsFile = toml::from_str(&data).map_err(|e| Error::Parse(e.to_string()))?;
    Ok(f)
}

fn flush_section(
    name: Option<String>,
    map: &mut HashMap<String, String>,
    out: &mut Vec<DvbV5Entry>,
) -> Result<()> {
    if let Some(name) = name {
        if !map.is_empty() {
            let ch =
                Channel::from_dvbv5_section(&name, map).map_err(Error::Parse)?;
            out.push(DvbV5Entry {
                channel: ch,
                raw: std::mem::take(map),
            });
        }
    }
    Ok(())
}

/// Parse legacy `dvbv5` / `dvb-format` `.conf` (UTF-8, `[Section]` + `KEY = VAL`).
pub fn parse_dvbv5_conf(path: &Path) -> Result<Vec<DvbV5Entry>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    let mut section_name: Option<String> = None;
    let mut map: HashMap<String, String> = HashMap::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush_section(section_name.take(), &mut map, &mut out)?;
            section_name = Some(line[1..line.len() - 1].to_string());
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if section_name.is_none() {
            return Err(Error::Parse("key=value before first [section]".into()));
        }
        map.insert(k.trim().to_string(), v.trim().to_string());
    }

    flush_section(section_name.take(), &mut map, &mut out)?;
    Ok(out)
}

pub fn find_entry<'a>(entries: &'a [DvbV5Entry], name: &str) -> Result<&'a DvbV5Entry> {
    entries
        .iter()
        .find(|e| e.channel.name == name)
        .ok_or_else(|| Error::ChannelNotFound(name.to_string()))
}

pub fn write_channels_json(path: &Path, file: &ChannelsFile) -> Result<()> {
    let s = serde_json::to_string_pretty(file).map_err(|e| Error::Parse(e.to_string()))?;
    fs::write(path, s)?;
    Ok(())
}
