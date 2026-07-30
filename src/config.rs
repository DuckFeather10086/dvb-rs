use crate::channel::{Channel, ChannelsFile};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// Channel plus raw `[section]` key/values for full DVBv5 tuning (`dvbv5-zap` compatibility).
#[derive(Debug, Clone)]
pub struct DvbV5Entry {
    /// Literal string inside `[...]` (may be legacy mojibake); still accepted for lookup.
    pub section_title: String,
    pub channel: Channel,
    /// Extra lookup strings from `DVBR_ALIASES` (comma / semicolon / `|` / `｜` separated, UTF-8).
    pub aliases: Vec<String>,
    pub raw: HashMap<String, String>,
}

impl DvbV5Entry {
    pub fn matches_lookup(&self, q: &str) -> bool {
        self.channel.name == q || self.section_title == q || self.aliases.iter().any(|a| a == q)
    }
}

fn parse_dvbr_aliases(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c == ';' || c == '|' || c == '｜')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// Modern channel list (JSON / TOML). Same tuning keys as dvbv5 `.conf` body, under `tuning` in JSON
/// or flat / `[tuning]` in TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelsDocument {
    #[serde(default = "channels_doc_version")]
    pub version: u32,
    pub channels: Vec<ChannelRecord>,
}

fn channels_doc_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRecord {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Original `[section]` title for `dvbv5-zap --channels legacy.conf` workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_zap_section: Option<String>,
    pub tuning: HashMap<String, String>,
}

/// Load [`ChannelsFile`] from JSON (minimal scan-style list, no `tuning` envelope).
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
            let section_title = name.clone();
            let aliases = map
                .get("DVBR_ALIASES")
                .map(|v| parse_dvbr_aliases(v))
                .unwrap_or_default();
            let ch = Channel::from_dvbv5_section(&section_title, map).map_err(Error::Parse)?;
            out.push(DvbV5Entry {
                section_title,
                channel: ch,
                aliases,
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
        .find(|e| e.matches_lookup(name))
        .ok_or_else(|| Error::ChannelNotFound(name.to_string()))
}

pub fn write_channels_json(path: &Path, file: &ChannelsFile) -> Result<()> {
    let s = serde_json::to_string_pretty(file).map_err(|e| Error::Parse(e.to_string()))?;
    fs::write(path, s)?;
    Ok(())
}

pub fn write_channels_document_json(path: &Path, doc: &ChannelsDocument) -> Result<()> {
    let s = serde_json::to_string_pretty(doc).map_err(|e| Error::Parse(e.to_string()))?;
    fs::write(path, s)?;
    Ok(())
}

fn channel_record_to_entry(r: ChannelRecord) -> Result<DvbV5Entry> {
    let mut aliases = r.aliases;
    if let Some(ref z) = r.legacy_zap_section {
        if z != &r.name && !aliases.contains(z) {
            aliases.push(z.clone());
        }
    }
    let ch = Channel::from_named_tuning(&r.name, &r.tuning).map_err(Error::Parse)?;
    Ok(DvbV5Entry {
        section_title: r.name.clone(),
        channel: ch,
        aliases,
        raw: r.tuning,
    })
}

/// Load tuning entries from `.conf`, `.json` ([`ChannelsDocument`]), or `.toml` (same schema).
pub fn load_channel_entries(path: &Path) -> Result<Vec<DvbV5Entry>> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "conf" => parse_dvbv5_conf(path),
        "json" => load_channel_entries_json(path),
        "toml" => load_channel_entries_toml(path),
        _ => Err(Error::Msg(format!(
            "unsupported channels path (use .conf / .json / .toml): {}",
            path.display()
        ))),
    }
}

/// Load the full channels.json document, preserving `aliases` /
/// `legacy_zap_section` / the whole `tuning` map. Use this (rather than
/// [`load_channel_entries`]) when the document is going to be written back.
pub fn load_channels_document(path: &Path) -> Result<ChannelsDocument> {
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|e| Error::Parse(e.to_string()))
}

fn load_channel_entries_json(path: &Path) -> Result<Vec<DvbV5Entry>> {
    let data = fs::read_to_string(path)?;
    let doc: ChannelsDocument =
        serde_json::from_str(&data).map_err(|e| Error::Parse(e.to_string()))?;
    doc.channels
        .into_iter()
        .map(channel_record_to_entry)
        .collect()
}

fn toml_scalar_to_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        _ => v.to_string(),
    }
}

fn load_channel_entries_toml(path: &Path) -> Result<Vec<DvbV5Entry>> {
    let text = fs::read_to_string(path)?;
    let v = text
        .parse::<toml::Value>()
        .map_err(|e: toml::de::Error| Error::Parse(e.to_string()))?;
    let arr = v
        .get("channels")
        .and_then(|c| c.as_array())
        .ok_or_else(|| Error::Parse("toml: top-level `channels` array required".into()))?;
    let mut out = Vec::new();
    for item in arr {
        let t = item
            .as_table()
            .ok_or_else(|| Error::Parse("toml: each channel must be a table".into()))?;
        let name = t
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| Error::Parse("toml: channel.name required".into()))?
            .to_string();
        let aliases: Vec<String> = t
            .get("aliases")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let legacy_zap_section = t
            .get("legacy_zap_section")
            .and_then(|x| x.as_str())
            .map(String::from);

        let mut tuning: HashMap<String, String> = HashMap::new();
        if let Some(sub) = t.get("tuning").and_then(|x| x.as_table()) {
            for (k, tv) in sub {
                tuning.insert(k.clone(), toml_scalar_to_string(tv));
            }
        }
        for (k, tv) in t {
            if matches!(
                k.as_str(),
                "name" | "aliases" | "legacy_zap_section" | "tuning" | "version"
            ) {
                continue;
            }
            tuning.insert(k.clone(), toml_scalar_to_string(tv));
        }

        out.push(channel_record_to_entry(ChannelRecord {
            name,
            aliases,
            legacy_zap_section,
            tuning,
        })?);
    }
    Ok(out)
}

fn strip_dvbr_meta(map: &mut HashMap<String, String>) {
    for k in ["DVBR_NAME", "DVBR_ALIASES", "CHANNEL_LABEL"] {
        map.remove(k);
    }
}

fn needs_generated_id(section_title: &str, channel_display: &str) -> bool {
    if section_title.contains('|') || section_title.contains('!') || section_title.contains('%') {
        return true;
    }
    if channel_display != section_title {
        return false;
    }
    let non_ascii = channel_display.chars().filter(|c| !c.is_ascii()).count();
    non_ascii > 2
}

fn unique_name(base: String, used: &mut HashSet<String>) -> String {
    if !used.contains(&base) {
        used.insert(base.clone());
        return base;
    }
    let mut n = 2u32;
    loop {
        let cand = format!("{base}_{n}");
        if !used.contains(&cand) {
            used.insert(cand.clone());
            return cand;
        }
        n += 1;
    }
}

/// Build a [`ChannelsDocument`] from parsed `.conf` entries (stable ids + optional legacy zap titles).
pub fn document_from_conf_entries(entries: Vec<DvbV5Entry>) -> ChannelsDocument {
    let mut used = HashSet::new();
    let mut channels = Vec::with_capacity(entries.len());
    for e in entries {
        let ascii_label = e
            .channel
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !e.channel.name.is_empty();
        let from_explicit_label = e.channel.name != e.section_title;

        let candidate = if from_explicit_label && ascii_label {
            e.channel.name.clone()
        } else if needs_generated_id(&e.section_title, &e.channel.name) {
            format!("u{}_{}", e.channel.service_id, e.channel.frequency)
        } else {
            e.channel.name.clone()
        };
        let name = unique_name(candidate, &mut used);

        let legacy_zap_section = (name != e.section_title).then(|| e.section_title.clone());

        let mut tuning = e.raw.clone();
        strip_dvbr_meta(&mut tuning);

        let mut aliases = e.aliases;
        if let Some(ref z) = legacy_zap_section {
            if !aliases.contains(z) {
                aliases.push(z.clone());
            }
        }

        channels.push(ChannelRecord {
            name,
            aliases,
            legacy_zap_section,
            tuning,
        });
    }
    ChannelsDocument {
        version: 1,
        channels,
    }
}
