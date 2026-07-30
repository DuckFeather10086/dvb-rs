use crate::channel::{Channel, ChannelsFile};
use crate::config::DvbV5Entry;
use crate::error::{Error, Result};
use crate::frontend::Frontend;
use crate::si_reader::{read_section, read_sections, SI_READ_TIMEOUT};
use crate::si_tables::{parse_pat, parse_sdt};
use crate::tuner::{build_props_simple_isdbt, tune_frontend, wait_lock};
use std::time::Duration;

/// PID / table_id pairs this scan reads.
const PAT_PID: u16 = 0x0000;
const PAT_TABLE_ID: u8 = 0x00;
const SDT_PID: u16 = 0x0011;
/// SDT for *this* transport stream (ARIB STD-B10 / EN 300 468).
const SDT_ACTUAL_TABLE_ID: u8 = 0x42;

/// What a merge changed, for reporting to the user.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Records whose `name` was an auto-generated placeholder and has been
    /// replaced with the broadcast service name.
    pub renamed: Vec<(String, String)>,
    /// Records that gained the service name as an alias.
    pub aliased: Vec<String>,
    /// Scanned services with no matching record in the document.
    pub unmatched: Vec<u16>,
}

/// True for names this tool (or the legacy migrate) invents when it has no
/// real service name to use: `program_1064`, `service_1064`,
/// `u1065_539142857`, `539.14MHz#1065`.
fn is_placeholder_name(name: &str) -> bool {
    if let Some(rest) = name
        .strip_prefix("program_")
        .or(name.strip_prefix("service_"))
    {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    if let Some(rest) = name.strip_prefix('u') {
        if let Some((sid, freq)) = rest.split_once('_') {
            return !sid.is_empty()
                && !freq.is_empty()
                && sid.chars().all(|c| c.is_ascii_digit())
                && freq.chars().all(|c| c.is_ascii_digit());
        }
    }
    name.contains("MHz#")
}

/// Fold freshly scanned service names into an existing channels.json
/// document, matching on SERVICE_ID + FREQUENCY.
///
/// Deliberately additive: a curated name (`asahi`, `NHK_G`, …) is left alone
/// because config files and `CHANNEL_MAP`-style tables reference it — the
/// decoded broadcast name is added as an alias instead. Only placeholder
/// names get replaced. Nothing is ever deleted, so a stale mojibake alias
/// keeps resolving for anything that still uses it.
pub fn merge_scanned_names(
    doc: &mut crate::config::ChannelsDocument,
    scanned: &ChannelsFile,
) -> MergeReport {
    let mut report = MergeReport::default();

    // Names already in use, so a replacement can't collide with one.
    let mut taken: Vec<String> = doc.channels.iter().map(|c| c.name.clone()).collect();

    for svc in &scanned.channels {
        if svc.name.is_empty() || is_placeholder_name(&svc.name) {
            continue;
        }
        let sid = svc.service_id.to_string();
        let freq = svc.frequency.to_string();

        let Some(rec) = doc.channels.iter_mut().find(|r| {
            r.tuning.get("SERVICE_ID").map(String::as_str) == Some(sid.as_str())
                && r.tuning.get("FREQUENCY").map(String::as_str) == Some(freq.as_str())
        }) else {
            report.unmatched.push(svc.service_id);
            continue;
        };

        if is_placeholder_name(&rec.name) {
            // Several services of one broadcaster share a service name
            // ("テレビ朝日" ×4), so suffix on collision the same way the
            // legacy migrate did (`TOKYO MX1_2`).
            let mut candidate = svc.name.clone();
            let mut n = 2;
            while taken.iter().any(|t| t == &candidate) {
                candidate = format!("{}_{}", svc.name, n);
                n += 1;
            }
            report.renamed.push((rec.name.clone(), candidate.clone()));
            taken.push(candidate.clone());
            // The old placeholder stays reachable as an alias.
            if !rec.aliases.contains(&rec.name) {
                rec.aliases.push(rec.name.clone());
            }
            rec.name = candidate;
        } else if rec.name != svc.name && !rec.aliases.contains(&svc.name) {
            rec.aliases.push(svc.name.clone());
            report.aliased.push(rec.name.clone());
        }
    }
    report
}

/// Lock to a transport (from full `.conf` entry or raw `frequency` / `bandwidth`), then read PAT+SDT once.
pub fn scan_current_transport(
    adapter: u32,
    fe_id: u32,
    dmx_id: u32,
    delivery: &str,
    frequency: u32,
    bandwidth_hz: u32,
    entry: Option<&DvbV5Entry>,
) -> Result<ChannelsFile> {
    let fe = Frontend::open_rw(adapter, fe_id)?;

    if let Some(e) = entry {
        tune_frontend(&fe, e)?;
    } else {
        let mut props = match delivery.to_uppercase().as_str() {
            "ISDBT" | "" => build_props_simple_isdbt(frequency, bandwidth_hz, 0),
            _ => {
                return Err(Error::Msg(format!(
                    "scan without .conf entry only supports ISDBT for now (got {delivery})"
                )));
            }
        };
        fe.set_properties(&mut props)?;
    }

    wait_lock(&fe, Duration::from_secs(15), Duration::from_millis(150))?;

    let tune_freq = entry.map(|e| e.channel.frequency).unwrap_or(frequency);
    let tune_bw = entry
        .map(|e| {
            if e.channel.bandwidth_hz > 0 {
                e.channel.bandwidth_hz
            } else {
                6_000_000
            }
        })
        .unwrap_or(if bandwidth_hz > 0 {
            bandwidth_hz
        } else {
            6_000_000
        });
    let del = entry
        .map(|e| e.channel.delivery.clone())
        .unwrap_or_else(|| delivery.to_string());

    // Both tables come off a PID tap reassembled in userspace — the kernel
    // section filter this used to use never returns on smsusb hardware.
    // See si_reader for the full story.
    let pat_section = read_section(adapter, dmx_id, PAT_PID, PAT_TABLE_ID, SI_READ_TIMEOUT)?;
    let pat = parse_pat(&pat_section)?;

    // Collect every SDT section: one section holds a limited number of
    // service descriptors, so a busy mux splits them and reading only the
    // first would silently drop services.
    let sdt_sections = read_sections(
        adapter,
        dmx_id,
        SDT_PID,
        SDT_ACTUAL_TABLE_ID,
        SI_READ_TIMEOUT,
        false,
    )?;

    let mut channels = Vec::new();
    let mut seen_service_ids = Vec::new();
    for section in &sdt_sections {
        let services = match parse_sdt(section) {
            Ok(s) => s,
            // One malformed section shouldn't lose the others.
            Err(e) => {
                eprintln!("scan: skipping bad SDT section: {e}");
                continue;
            }
        };
        for s in services {
            if seen_service_ids.contains(&s.service_id) {
                continue;
            }
            seen_service_ids.push(s.service_id);

            let mut name = s.name;
            if name.is_empty() {
                name = format!("service_{}", s.service_id);
            }
            channels.push(Channel {
                name,
                delivery: del.clone(),
                frequency: tune_freq,
                bandwidth_hz: tune_bw,
                service_id: s.service_id,
                video_pids: Vec::new(),
                audio_pids: Vec::new(),
            });
        }
    }

    if channels.is_empty() {
        for p in pat {
            if p.program_number == 0 {
                continue;
            }
            channels.push(Channel {
                name: format!("program_{}", p.program_number),
                delivery: del.clone(),
                frequency: tune_freq,
                bandwidth_hz: tune_bw,
                service_id: p.program_number,
                video_pids: Vec::new(),
                audio_pids: Vec::new(),
            });
        }
    }

    Ok(ChannelsFile { channels })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRecord, ChannelsDocument};
    use std::collections::HashMap;

    fn rec(name: &str, sid: u16, freq: u32, aliases: &[&str]) -> ChannelRecord {
        let mut tuning = HashMap::new();
        tuning.insert("SERVICE_ID".to_string(), sid.to_string());
        tuning.insert("FREQUENCY".to_string(), freq.to_string());
        ChannelRecord {
            name: name.to_string(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            legacy_zap_section: None,
            tuning,
        }
    }

    fn scanned(items: &[(&str, u16, u32)]) -> ChannelsFile {
        ChannelsFile {
            channels: items
                .iter()
                .map(|(name, sid, freq)| Channel {
                    name: name.to_string(),
                    delivery: "ISDBT".into(),
                    frequency: *freq,
                    bandwidth_hz: 6_000_000,
                    service_id: *sid,
                    video_pids: Vec::new(),
                    audio_pids: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn placeholder_names_are_recognized() {
        for n in [
            "program_1064",
            "service_23608",
            "u1065_539142857",
            "539.14MHz#1065",
        ] {
            assert!(is_placeholder_name(n), "{n} should be a placeholder");
        }
        for n in ["asahi", "NHK_G", "TOKYO MX1", "テレビ朝日", "tvk1", "u"] {
            assert!(!is_placeholder_name(n), "{n} should be kept");
        }
    }

    // A curated name is what configs and CHANNEL_MAP reference — merging must
    // not rename it out from under them, only add the broadcast name.
    #[test]
    fn curated_name_is_kept_and_gains_an_alias() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![rec("asahi", 1064, 539142857, &["|ÆìÓD+F|"])],
        };
        let report = merge_scanned_names(&mut doc, &scanned(&[("テレビ朝日", 1064, 539142857)]));

        assert_eq!(doc.channels[0].name, "asahi");
        assert!(doc.channels[0].aliases.contains(&"テレビ朝日".to_string()));
        // Nothing is deleted: the old mojibake alias still resolves.
        assert!(doc.channels[0].aliases.contains(&"|ÆìÓD+F|".to_string()));
        assert_eq!(report.aliased, vec!["asahi".to_string()]);
        assert!(report.renamed.is_empty());
    }

    #[test]
    fn placeholder_is_renamed_and_collisions_get_suffixed() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![
                rec("u1065_539142857", 1065, 539142857, &[]),
                rec("u1066_539142857", 1066, 539142857, &[]),
            ],
        };
        // Both services broadcast the same service name.
        let report = merge_scanned_names(
            &mut doc,
            &scanned(&[
                ("テレビ朝日", 1065, 539142857),
                ("テレビ朝日", 1066, 539142857),
            ]),
        );

        assert_eq!(doc.channels[0].name, "テレビ朝日");
        assert_eq!(doc.channels[1].name, "テレビ朝日_2");
        // The old placeholder stays reachable.
        assert!(doc.channels[0]
            .aliases
            .contains(&"u1065_539142857".to_string()));
        assert_eq!(report.renamed.len(), 2);
    }

    #[test]
    fn collision_with_an_existing_curated_name_is_suffixed() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![
                rec("テレビ朝日", 1064, 539142857, &[]),
                rec("u1065_539142857", 1065, 539142857, &[]),
            ],
        };
        merge_scanned_names(&mut doc, &scanned(&[("テレビ朝日", 1065, 539142857)]));
        assert_eq!(doc.channels[1].name, "テレビ朝日_2");
    }

    // Same service_id on a different transport must not be confused.
    #[test]
    fn matching_requires_both_service_id_and_frequency() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![rec("other-mux", 1064, 473142857, &[])],
        };
        let report = merge_scanned_names(&mut doc, &scanned(&[("テレビ朝日", 1064, 539142857)]));

        assert_eq!(doc.channels[0].name, "other-mux");
        assert!(doc.channels[0].aliases.is_empty());
        assert_eq!(report.unmatched, vec![1064]);
    }

    #[test]
    fn merge_is_idempotent() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![rec("asahi", 1064, 539142857, &[])],
        };
        let s = scanned(&[("テレビ朝日", 1064, 539142857)]);
        merge_scanned_names(&mut doc, &s);
        let after_first = doc.channels[0].aliases.clone();
        let report = merge_scanned_names(&mut doc, &s);
        assert_eq!(doc.channels[0].aliases, after_first);
        assert!(report.aliased.is_empty());
    }

    // A scan that fell back to PAT placeholders must not clobber real names.
    #[test]
    fn scanned_placeholders_are_ignored() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![rec("u1065_539142857", 1065, 539142857, &[])],
        };
        let report = merge_scanned_names(&mut doc, &scanned(&[("program_1065", 1065, 539142857)]));
        assert_eq!(doc.channels[0].name, "u1065_539142857");
        assert_eq!(report, MergeReport::default());
    }
}
