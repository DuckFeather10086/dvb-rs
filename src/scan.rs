use crate::channel::{Channel, ChannelsFile};
use crate::config::DvbV5Entry;
use crate::error::{Error, Result};
use crate::frontend::Frontend;
use crate::si_reader::{read_section, read_sections, SI_READ_TIMEOUT};
use crate::si_tables::{parse_pat, parse_sdt};
use crate::tuner::{build_props_simple_isdbt, tune_frontend, wait_lock};
use std::collections::HashMap;
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
    /// Records created for scanned services the document did not have —
    /// only ever non-empty when the caller passed `add_new`.
    pub added: Vec<String>,
    /// Scanned services skipped because the scan had no real name for them:
    /// the SDT could not be read and these are PAT program numbers. Never
    /// added, whatever `add_new` says — see `merge_scanned`.
    pub nameless: Vec<u16>,
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
    merge_scanned(doc, scanned, false)
}

/// [`merge_scanned_names`], optionally *creating* a record for each scanned
/// service the document does not have.
///
/// Folding names is all a merge needs to do when the document already
/// describes every mux — auditing a curated list, or re-scanning one
/// transport after the broadcaster renamed a service. Building the list in
/// the first place is the other job, and it is the one a fresh install has:
/// sweep the band, and each transport that locks contributes its services.
/// Without `add_new` that sweep folds fifty muxes' names into a document
/// that has no records to fold them into and writes nothing.
///
/// A service is only added when the scan learned its real name from the
/// SDT. A placeholder means the SDT could not be read and the names are PAT
/// program numbers, which is not enough to decide what belongs in a channel
/// list: `515.14MHz#23864` was exactly that — in the PAT, absent from the
/// SDT, no EIT, zero bytes when tuned. They are reported in
/// [`MergeReport::nameless`] so a mux that produced nothing says why.
pub fn merge_scanned(
    doc: &mut crate::config::ChannelsDocument,
    scanned: &ChannelsFile,
    add_new: bool,
) -> MergeReport {
    let mut report = MergeReport::default();

    // Every string that already selects some record — names *and* aliases.
    //
    // Aliases have to count. Lookup (`Channels::find` / config::find_entry)
    // walks records in order and checks each one's name and aliases together,
    // so the first record wins. Handing a placeholder a name that is already
    // an alias of an earlier record produces a record that cannot be selected
    // by its own name: the earlier record answers instead. That happened for
    // real — service 1065 was renamed "テレビ朝日", which was already an
    // alias of `asahi` (1064), so 1065 became unreachable and its guide came
    // back empty while tuning it went to 1064.
    let mut taken: Vec<String> = doc
        .channels
        .iter()
        .flat_map(|c| {
            std::iter::once(c.name.clone())
                .chain(c.aliases.iter().cloned())
                .chain(c.legacy_zap_section.clone())
        })
        .collect();

    for svc in &scanned.channels {
        if svc.name.is_empty() || is_placeholder_name(&svc.name) {
            report.nameless.push(svc.service_id);
            continue;
        }
        let sid = svc.service_id.to_string();
        let freq = svc.frequency.to_string();

        // Resolve to an index rather than holding a `&mut` from `find`: the
        // not-found arm below pushes onto the same Vec.
        let found = doc.channels.iter().position(|r| {
            r.tuning.get("SERVICE_ID").map(String::as_str) == Some(sid.as_str())
                && r.tuning.get("FREQUENCY").map(String::as_str) == Some(freq.as_str())
        });
        let Some(idx) = found else {
            report.unmatched.push(svc.service_id);
            if add_new {
                let name = unique_name(&svc.name, &taken);
                taken.push(name.clone());
                doc.channels.push(new_record(&name, svc));
                report.added.push(name);
            }
            continue;
        };
        let rec = &mut doc.channels[idx];

        if is_placeholder_name(&rec.name) {
            // Several services of one broadcaster share a service name
            // ("テレビ朝日" ×4), so suffix on collision the same way the
            // legacy migrate did (`TOKYO MX1_2`).
            let candidate = unique_name(&svc.name, &taken);
            report.renamed.push((rec.name.clone(), candidate.clone()));
            taken.push(candidate.clone());
            taken.push(rec.name.clone());
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

/// `base`, or `base_2`, `base_3`… — the first form nothing in `taken`
/// already answers to.
///
/// Four services of one broadcaster share a service name ("テレビ朝日" ×4),
/// and lookup is first-match-wins over every record's name *and* aliases, so
/// a duplicate is not a cosmetic problem: the second record becomes
/// unreachable by its own name. Suffixing is what the legacy migrate did and
/// what the rest of the file already looks like.
fn unique_name(base: &str, taken: &[String]) -> String {
    let mut candidate = base.to_string();
    let mut n = 2;
    while taken.iter().any(|t| t == &candidate) {
        candidate = format!("{base}_{n}");
        n += 1;
    }
    candidate
}

/// A channel record for a service the document did not have.
///
/// Only the four keys a tune actually needs. The rest of what a legacy
/// `.conf` carries (layer modulation, interleaving, guard interval) is
/// AUTO-detected by the frontend — `build_props_from_dvbv5` supplies
/// INVERSION/GUARD/TRANSMISSION defaults when the record is silent — so
/// writing them out would be recording the demodulator's guesses as
/// configuration.
fn new_record(name: &str, svc: &Channel) -> crate::config::ChannelRecord {
    let mut tuning = HashMap::new();
    tuning.insert("DELIVERY_SYSTEM".to_string(), svc.delivery.clone());
    tuning.insert("FREQUENCY".to_string(), svc.frequency.to_string());
    tuning.insert(
        "BANDWIDTH_HZ".to_string(),
        if svc.bandwidth_hz > 0 {
            svc.bandwidth_hz
        } else {
            6_000_000
        }
        .to_string(),
    );
    tuning.insert("SERVICE_ID".to_string(), svc.service_id.to_string());
    crate::config::ChannelRecord {
        name: name.to_string(),
        aliases: Vec::new(),
        legacy_zap_section: None,
        tuning,
    }
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

    // Regression: a placeholder must not be handed a name that is already an
    // ALIAS of an earlier record. Lookup checks each record's name and
    // aliases together in file order, so the earlier record would answer and
    // this one would be unreachable by its own name.
    #[test]
    fn rename_does_not_shadow_an_earlier_records_alias() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![
                rec("asahi", 1064, 539142857, &["テレビ朝日"]),
                rec("u1065_539142857", 1065, 539142857, &[]),
                rec("u1066_539142857", 1066, 539142857, &[]),
            ],
        };
        merge_scanned_names(
            &mut doc,
            &scanned(&[
                ("テレビ朝日", 1065, 539142857),
                ("テレビ朝日", 1066, 539142857),
            ]),
        );

        assert_eq!(doc.channels[0].name, "asahi");
        assert_eq!(
            doc.channels[1].name, "テレビ朝日_2",
            "the plain name already resolves to asahi, so 1065 must be suffixed"
        );
        assert_eq!(doc.channels[2].name, "テレビ朝日_3");

        // Every record must be selectable by its own name, first-match wins.
        for r in &doc.channels {
            let first = doc
                .channels
                .iter()
                .find(|c| c.name == r.name || c.aliases.contains(&r.name))
                .unwrap();
            assert_eq!(
                first.tuning.get("SERVICE_ID"),
                r.tuning.get("SERVICE_ID"),
                "{} resolves to a different service",
                r.name
            );
        }
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

    // Building a list from nothing: the sweep's whole job. Without add_new
    // this writes an empty document and the install is stuck at step one.
    #[test]
    fn add_new_creates_records_from_an_empty_document() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![],
        };
        let report = merge_scanned(
            &mut doc,
            &scanned(&[("NHK総合", 1024, 557142857), ("NHKEテレ", 1032, 551142857)]),
            true,
        );

        assert_eq!(report.added, vec!["NHK総合", "NHKEテレ"]);
        assert_eq!(doc.channels.len(), 2);
        // Enough to tune with, and nothing the demodulator works out on its own.
        let t = &doc.channels[0].tuning;
        assert_eq!(t.get("SERVICE_ID").map(String::as_str), Some("1024"));
        assert_eq!(t.get("FREQUENCY").map(String::as_str), Some("557142857"));
        assert_eq!(t.get("DELIVERY_SYSTEM").map(String::as_str), Some("ISDBT"));
        assert_eq!(t.get("BANDWIDTH_HZ").map(String::as_str), Some("6000000"));
    }

    // A second sweep over a list somebody has since tidied must leave the
    // tidying alone — no duplicates, no renames.
    #[test]
    fn add_new_is_idempotent_and_keeps_curated_names() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![rec("asahi", 1064, 539142857, &[])],
        };
        let s = scanned(&[("テレビ朝日", 1064, 539142857), ("テレ玉", 1000, 485142857)]);

        let first = merge_scanned(&mut doc, &s, true);
        assert_eq!(first.added, vec!["テレ玉"]);
        assert_eq!(doc.channels[0].name, "asahi", "curated name must survive");
        assert!(doc.channels[0].aliases.contains(&"テレビ朝日".to_string()));

        let second = merge_scanned(&mut doc, &s, true);
        assert!(second.added.is_empty(), "a rescan must not duplicate");
        assert!(second.aliased.is_empty());
        assert_eq!(doc.channels.len(), 2);
    }

    // A new record must not be given a name an earlier record already
    // answers to, for the same reason a rename must not: lookup is
    // first-match-wins over names and aliases together.
    #[test]
    fn added_names_do_not_shadow_existing_records() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![rec("asahi", 1064, 539142857, &["テレビ朝日"])],
        };
        // Same service name on a different mux, and twice within the scan.
        let report = merge_scanned(
            &mut doc,
            &scanned(&[
                ("テレビ朝日", 2000, 473142857),
                ("テレビ朝日", 2001, 473142857),
            ]),
            true,
        );

        assert_eq!(report.added, vec!["テレビ朝日_2", "テレビ朝日_3"]);
        for r in &doc.channels {
            let first = doc
                .channels
                .iter()
                .find(|c| c.name == r.name || c.aliases.contains(&r.name))
                .unwrap();
            assert_eq!(
                first.tuning.get("SERVICE_ID"),
                r.tuning.get("SERVICE_ID"),
                "{} resolves to a different service",
                r.name
            );
        }
    }

    // The SDT is what decides membership. A PAT-only mux contributes
    // nothing, and says which service ids it passed over.
    #[test]
    fn add_new_will_not_invent_records_from_pat_program_numbers() {
        let mut doc = ChannelsDocument {
            version: 1,
            channels: vec![],
        };
        let report = merge_scanned(
            &mut doc,
            &scanned(&[("program_23864", 23864, 515142857)]),
            true,
        );
        assert!(doc.channels.is_empty());
        assert!(report.added.is_empty());
        assert_eq!(report.nameless, vec![23864]);
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
        // Nothing changed about the document; the service is reported as one
        // the SDT did not name, which is why it was passed over.
        assert_eq!(
            MergeReport {
                nameless: vec![1065],
                ..MergeReport::default()
            },
            report
        );
    }
}
