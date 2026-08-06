use clap::{Parser, Subcommand};
use dvb_rs::channel::ChannelsFile;
use dvb_rs::config;
use dvb_rs::demux::{Demux, DvrReader, PidTaps};
use dvb_rs::eit::TsSectionAssembler;
use dvb_rs::eit::{self, EitEvent, EitSection};
use dvb_rs::frontend::Frontend;
use dvb_rs::scan;
use dvb_rs::si_reader;
use dvb_rs::si_tables::{parse_pat, parse_pmt};
use dvb_rs::signal;
use dvb_rs::sys::ffi;
use dvb_rs::tuner::{tune_frontend, wait_lock};
use log::{info, warn};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BUF: usize = 188 * 512;
const MAX_EPG_COLLECT_SECS: u64 = 60;
const SI_PID_READ_TIMEOUT: Duration = Duration::from_secs(5);
const DVR_STALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(name = "dvb-rs")]
#[command(about = "Rust DVBv5-style tune/scan (Linux DVB API v5)", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Tune frontend and stream full TS (like `dvbv5-zap -P -o -`)
    Tune {
        #[arg(short = 'a', long, default_value_t = 0)]
        adapter: u32,
        #[arg(short = 'f', long, default_value_t = 0)]
        frontend: u32,
        #[arg(short = 'd', long, default_value_t = 0)]
        demux: u32,
        #[arg(short = 'c', long)]
        channels: PathBuf,
        #[arg(short = 'o', long, default_value = "-")]
        output: String,
        #[arg(long, default_value_t = 15000)]
        lock_timeout_ms: u64,
        /// Opt back into full mux / all-PIDs. Avoid this on smsusb unless debugging.
        #[arg(long)]
        full_mux: bool,
        /// Channel id: `name` / alias in `channels.json`, or legacy `.conf` section / DVBR_* labels
        name: String,
    },
    /// Read one PAT + SDT after locking (non blind-scan)
    Scan {
        #[arg(short = 'a', long, default_value_t = 0)]
        adapter: u32,
        #[arg(short = 'f', long, default_value_t = 0)]
        frontend: u32,
        #[arg(short = 'd', long, default_value_t = 0)]
        demux: u32,
        #[arg(short = 'c', long)]
        channels: Option<PathBuf>,
        /// For simple scan without `.conf`: frequency (Hz)
        #[arg(long)]
        frequency: Option<u32>,
        #[arg(long, default_value_t = 6_000_000u32)]
        bandwidth_hz: u32,
        #[arg(long, default_value = "ISDBT")]
        delivery: String,
        #[arg(short = 'o', long, default_value = "channels.json")]
        output: PathBuf,
        /// If `--channels` is set: lookup name (`DVBR_NAME`, alias, or `[section]`)
        #[arg(long)]
        name: Option<String>,
        /// Fold the scanned service names into an existing channels.json at
        /// `--output` instead of overwriting it with just this transport.
        /// Additive: curated names are kept and gain the broadcast name as an
        /// alias; only auto-generated placeholders get renamed.
        #[arg(long, default_value_t = false)]
        merge: bool,
        /// With `--merge`: also create a record for each scanned service the
        /// document does not have. This is what builds a channel list from
        /// nothing — sweep the band and merge each transport that locks —
        /// where a plain `--merge` only folds names into records that already
        /// exist. Services the SDT did not name are never added.
        #[arg(long, default_value_t = false)]
        add_new: bool,
        /// Allow a non-merge write to replace an existing channel list. Without
        /// this, refusing is the default: the result would hold only the mux
        /// just scanned.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Show `FE_GET_INFO` name + caps summary
    FeInfo {
        #[arg(short = 'a', long, default_value_t = 0)]
        adapter: u32,
        #[arg(short = 'f', long, default_value_t = 0)]
        frontend: u32,
    },
    /// Dump legacy `.conf` to UTF-8 JSON channel list
    ConvertConf {
        #[arg(short = 'i', long)]
        input: PathBuf,
        #[arg(short = 'o', long)]
        output: PathBuf,
    },
    /// Migrate legacy `.conf` to `channels.json` / `channels.toml` style [`ChannelsDocument`]
    Migrate {
        #[arg(short = 'i', long)]
        input: PathBuf,
        #[arg(short = 'o', long, default_value = "channels.json")]
        output: PathBuf,
    },
    /// Fetch and display the EPG (Electronic Programme Guide) from EIT on PID 0x0012
    Epg {
        #[arg(short = 'a', long, default_value_t = 0)]
        adapter: u32,
        #[arg(short = 'f', long, default_value_t = 0)]
        frontend: u32,
        #[arg(short = 'd', long, default_value_t = 0)]
        demux: u32,
        #[arg(short = 'c', long)]
        channels: PathBuf,
        #[arg(long, default_value_t = 15000)]
        lock_timeout_ms: u64,
        /// Collect EIT schedule (0x50–0x5F) in addition to present/following (0x4E)
        #[arg(long)]
        schedule: bool,
        /// Total seconds to collect EIT sections (default 10 for p/f, 60 for schedule)
        #[arg(long)]
        collect_secs: Option<u64>,
        /// Output raw events as JSON
        #[arg(long)]
        json: bool,
        /// Emit events with no title and no synopsis (broadcast placeholders / not filled yet)
        #[arg(long)]
        include_empty: bool,
        /// Report only the named service. By default the whole mux is
        /// harvested: EIT on PID 0x0012 describes every service in the
        /// transport stream, so one tune covers all of a broadcaster's
        /// subchannels for free. Each event carries its own service_id.
        #[arg(long)]
        only_service: bool,
        /// Channel name (alias or dvbr_name in channels.json)
        name: String,
    },
}

struct AdapterLock {
    _file: File,
}

fn acquire_adapter_lock(adapter: u32) -> dvb_rs::Result<Option<AdapterLock>> {
    if std::env::var_os("DVBR_SKIP_ADAPTER_LOCK").is_some() {
        return Ok(None);
    }

    let path = format!("/tmp/dvbr-adapter{adapter}.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(AdapterLock { _file: file }));
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN) {
        Err(dvb_rs::Error::Msg(format!(
            "DVB adapter {adapter} is already in use (lock: {path})"
        )))
    } else {
        Err(err.into())
    }
}

fn service_pids_from_pat_pmt(adapter: u32, demux: u32, service_id: u16) -> dvb_rs::Result<Vec<u16>> {
    let pat_section = si_reader::read_section(adapter, demux, 0x0000, 0x00, SI_PID_READ_TIMEOUT)?;
    let pat = parse_pat(&pat_section)?;
    let pmt_pid = pat
        .iter()
        .find(|program| program.program_number == service_id)
        .map(|program| program.pid)
        .ok_or_else(|| dvb_rs::Error::Si(format!("service_id {service_id} not found in PAT")))?;

    let pmt_section = si_reader::read_section(adapter, demux, pmt_pid, 0x02, SI_PID_READ_TIMEOUT)?;
    let pmt = parse_pmt(&pmt_section)?;
    let mut pids = vec![0x0000, pmt_pid];
    if pmt.pcr_pid != 0x1fff {
        pids.push(pmt.pcr_pid);
    }
    pids.extend(pmt.ca_pids);
    for stream in pmt.streams {
        pids.push(stream.pid);
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn bounded_epg_collect_secs(schedule: bool, collect_secs: Option<u64>) -> dvb_rs::Result<u64> {
    let secs = collect_secs.unwrap_or(if schedule { 60 } else { 15 });
    if secs == 0 {
        return Err(dvb_rs::Error::Msg("--collect-secs must be at least 1".into()));
    }
    if secs > MAX_EPG_COLLECT_SECS {
        warn!("capping EPG collection to {MAX_EPG_COLLECT_SECS}s (requested {secs}s)");
        return Ok(MAX_EPG_COLLECT_SECS);
    }
    Ok(secs)
}

fn main() -> dvb_rs::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Tune {
            adapter,
            frontend,
            demux,
            channels,
            output,
            lock_timeout_ms,
            full_mux,
            name,
        } => {
            let _lock = acquire_adapter_lock(adapter)?;
            let entries = config::load_channel_entries(&channels)?;
            let entry = config::find_entry(&entries, &name)?;
            let fe = Frontend::open_rw(adapter, frontend)?;
            tune_frontend(&fe, entry)?;
            wait_lock(
                &fe,
                Duration::from_millis(lock_timeout_ms),
                Duration::from_millis(100),
            )?;

            let _full_mux_demux;
            let _pid_taps;
            if full_mux {
                warn!("starting full mux / all-PIDs tap; avoid long runs on smsusb");
                let dm = Demux::open_rw(adapter, demux)?;
                dm.zap_all_pids_to_dvr()?;
                _full_mux_demux = Some(dm);
                _pid_taps = None;
            } else {
                let pids = service_pids_from_pat_pmt(adapter, demux, entry.channel.service_id)?;
                info!(
                    "locked; starting service PID taps for service_id={} pids={:?}",
                    entry.channel.service_id, pids
                );
                let taps = PidTaps::start(adapter, demux, &pids)?;
                info!("started {} PID taps", taps.len());
                _full_mux_demux = None;
                _pid_taps = Some(taps);
            }
            let mut dvr = DvrReader::open_ro(adapter, demux)?;
            let mut buf = vec![0u8; BUF];

            let mut out: Box<dyn Write> = if output == "-" {
                Box::new(io::stdout().lock())
            } else {
                Box::new(File::create(&output)?)
            };

            let mut last_data = Instant::now();
            loop {
                let n = match dvr.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(e.into());
                    }
                };
                if n == 0 {
                    if last_data.elapsed() >= DVR_STALL_TIMEOUT {
                        return Err(format!(
                            "DVR delivered no data for {}s; aborting (signal loss or driver hang?)",
                            DVR_STALL_TIMEOUT.as_secs()
                        )
                        .into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                last_data = Instant::now();
                out.write_all(&buf[..n])?;
                let _ = out.flush();
            }
        }
        Cmd::Scan {
            adapter,
            frontend,
            demux,
            channels,
            frequency,
            bandwidth_hz,
            delivery,
            output,
            name,
            merge,
            add_new,
            force,
        } => {
            let _lock = acquire_adapter_lock(adapter)?;
            let (entry, freq, bw) = if let Some(path) = channels {
                let entries = config::load_channel_entries(&path)?;
                let n =
                    name.ok_or_else(|| dvb_rs::Error::Msg("--name required with --channels".into()))?;
                let e = config::find_entry(&entries, &n)?;
                (Some(e.clone()), e.channel.frequency, e.channel.bandwidth_hz)
            } else {
                let f = frequency.ok_or_else(|| {
                    dvb_rs::Error::Msg("either --channels + --name or --frequency is required".into())
                })?;
                (None, f, bandwidth_hz)
            };
            let entry_ref = entry.as_ref();
            let ch = scan::scan_current_transport(
                adapter,
                frontend,
                demux,
                &delivery,
                freq,
                if bw > 0 { bw } else { 6_000_000 },
                entry_ref,
            )?;
            if merge {
                let mut doc = config::load_channels_document(&output)?;
                let report = scan::merge_scanned(&mut doc, &ch, add_new);
                config::write_channels_document_json(&output, &doc)?;
                for (from, to) in &report.renamed {
                    info!("renamed {from} -> {to}");
                }
                for name in &report.aliased {
                    info!("added broadcast name as alias on {name}");
                }
                for name in &report.added {
                    info!("added {name}");
                }
                if !report.added.is_empty() {
                    // Nothing else prints the record count, and after a
                    // band sweep it is the number you actually want.
                    info!("{} record(s) in {}", doc.channels.len(), output.display());
                }
                if !add_new && !report.unmatched.is_empty() {
                    info!(
                        "scanned service(s) with no matching record: {:?} \
                         (--add-new would create them)",
                        report.unmatched
                    );
                }
                if !report.nameless.is_empty() {
                    // PAT program numbers: the SDT could not be read, so
                    // there is no name and no way to tell a service from a
                    // data carousel. Never added; say so rather than
                    // reporting a mux as empty.
                    info!(
                        "service(s) the SDT did not name, skipped: {:?}",
                        report.nameless
                    );
                }
                info!(
                    "merged {} scanned service(s) into {} ({} added, {} renamed, {} aliased)",
                    ch.channels.len(),
                    output.display(),
                    report.added.len(),
                    report.renamed.len(),
                    report.aliased.len()
                );
            } else {
                // Without --merge this writes *only* the scanned transport,
                // so pointing it at a populated channel list replaces every
                // other mux, plus the curated names and aliases, with one
                // frequency's worth of bare records. --output defaults to
                // channels.json, which makes that the easiest mistake in
                // this CLI to make and the least obvious to notice.
                if !force && config::load_channels_document(&output).is_ok() {
                    return Err(dvb_rs::Error::Msg(format!(
                        "{} is an existing channel list: `scan` without --merge would \
                         replace all of it with just this transport. Use --merge to fold \
                         the scanned names in, -o to write elsewhere, or --force if you \
                         really mean to overwrite.",
                        output.display()
                    )));
                }
                config::write_channels_json(&output, &ch)?;
                info!(
                    "wrote {} channel(s) to {}",
                    ch.channels.len(),
                    output.display()
                );
            }
        }
        Cmd::FeInfo { adapter, frontend } => {
            let fe = Frontend::open_rw(adapter, frontend)?;
            let mut info = ffi::dvb_frontend_info::default();
            fe.get_frontend_info(&mut info)?;
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }.to_string_lossy();
            println!("{name}");
            println!("caps (enum bitfield): {:?}", info.caps);
            let st = signal::read_stats(&fe)?;
            println!("status mask: 0x{:08x} (HAS_LOCK=0x10)", st.lock_mask);
            if let Some(s) = st.signal_strength_0_ffff {
                println!("signal (legacy ioctl): {s}");
            }
            if let Some(s) = st.snr_0_ffff {
                println!("snr (legacy ioctl): {s}");
            }
        }
        Cmd::ConvertConf { input, output } => {
            let entries = config::parse_dvbv5_conf(&input)?;
            let file = ChannelsFile {
                channels: entries.into_iter().map(|e| e.channel).collect(),
            };
            config::write_channels_json(&output, &file)?;
            info!("converted {} entries", file.channels.len());
        }
        Cmd::Migrate { input, output } => {
            let entries = config::parse_dvbv5_conf(&input)?;
            let n = entries.len();
            let doc = config::document_from_conf_entries(entries);
            if output
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase()
                == "toml"
            {
                let s =
                    toml::to_string_pretty(&doc).map_err(|e| dvb_rs::Error::Parse(e.to_string()))?;
                std::fs::write(&output, s)?;
            } else {
                config::write_channels_document_json(&output, &doc)?;
            }
            info!("migrated {} channels -> {}", n, output.display());
        }
        Cmd::Epg {
            adapter,
            frontend,
            demux,
            channels,
            lock_timeout_ms,
            schedule,
            collect_secs,
            json,
            include_empty,
            only_service,
            name,
        } => {
            let _lock = acquire_adapter_lock(adapter)?;
            let entries = config::load_channel_entries(&channels)?;
            let entry = config::find_entry(&entries, &name)?;
            let service_id = entry.channel.service_id;

            let fe = Frontend::open_rw(adapter, frontend)?;
            tune_frontend(&fe, entry)?;
            wait_lock(
                &fe,
                Duration::from_millis(lock_timeout_ms),
                Duration::from_millis(100),
            )?;
            info!("locked on {}; service_id={}", name, service_id);

            let secs = bounded_epg_collect_secs(schedule, collect_secs)?;
            let deadline = Instant::now() + Duration::from_secs(secs);

            // The Siano USB driver does not support kernel section filter (DMX_SET_FILTER) for
            // EIT on PID 0x0012. Use PES tap filters instead: this routes only the EIT PIDs to
            // the DVR device, reducing USB bandwidth and packet loss compared to the full-mux
            // approach, while we reassemble sections in userspace.
            //
            // Both EIT PIDs are tapped: 0x0012 carries the guide for the regular services and
            // 0x0027 the one for partial-reception (ワンセグ / 携帯) ones. They are separate
            // streams, so tapping only the first left every one-seg service with an empty
            // schedule.
            let eit_pids = [eit::EIT_PID, eit::EIT_PID_ONESEG];
            let _taps = PidTaps::start(adapter, demux, &eit_pids)?;
            let mut dvr = DvrReader::open_ro(adapter, demux)?;

            let mut all_sections: Vec<EitSection> = Vec::new();
            let mut seen: HashSet<(u16, u8, u8)> = HashSet::new();

            // Default: harvest the whole mux. EIT carries every service in
            // this transport stream, so restricting to the named one threw
            // away the siblings' guide for nothing.
            let only = if only_service { Some(service_id) } else { None };
            info!(
                "collecting EIT for {secs}s (PES tap on PID 0x0012 + 0x0027, userspace assembly){}...",
                if only.is_some() {
                    format!(", service {service_id} only")
                } else {
                    ", all services on this mux".to_string()
                }
            );
            collect_eit_from_dvr(
                &mut dvr,
                &eit_pids,
                only,
                schedule,
                deadline,
                &mut all_sections,
                &mut seen,
            );
            drop(_taps);

            // Gather all events, deduplicate by event_id (keep the richest copy: same id is
            // often sent as TBC placeholder first, then filled in later sections / repetitions).
            // Keyed by (service, event) — event_id is only unique within a
            // service, so keying on it alone made services on the same mux
            // overwrite each other's programmes.
            let mut events: HashMap<(u16, u16), EitEvent> = HashMap::new();
            for sec in &all_sections {
                for ev in &sec.events {
                    let score = |e: &EitEvent| {
                        e.title.chars().count()
                            + e.text.chars().count()
                            + e.content_nibbles.len().saturating_mul(8)
                    };
                    events
                        .entry((sec.service_id, ev.event_id))
                        .and_modify(|existing| {
                            if score(ev) > score(existing) {
                                *existing = ev.clone();
                            }
                        })
                        .or_insert_with(|| ev.clone());
                }
            }
            let mut sorted: Vec<(u16, EitEvent)> =
                events.into_iter().map(|((sid, _), ev)| (sid, ev)).collect();
            sorted.sort_by(|a, b| a.1.start.cmp(&b.1.start).then(a.0.cmp(&b.0)));
            if !include_empty {
                sorted.retain(|(_, ev)| !ev.title.trim().is_empty() || !ev.text.trim().is_empty());
            }
            info!(
                "{} events across {} service(s)",
                sorted.len(),
                sorted
                    .iter()
                    .map(|(sid, _)| *sid)
                    .collect::<BTreeSet<_>>()
                    .len()
            );

            if json {
                print_events_json(&sorted);
            } else {
                print_events_human(&sorted);
            }
        }
    }
    Ok(())
}

// ── EIT collection ───────────────────────────────────────────────────────────

/// Collect EIT sections from the DVR tap until `deadline`.
///
/// `only_service` restricts the harvest to one service_id; `None` (the
/// default) keeps the whole transport stream, which is what makes one tune
/// yield the guide for every service on the mux.
fn collect_eit_from_dvr(
    dvr: &mut DvrReader,
    pids: &[u16],
    only_service: Option<u16>,
    include_schedule: bool,
    deadline: Instant,
    out: &mut Vec<EitSection>,
    seen: &mut HashSet<(u16, u8, u8)>,
) {
    // One assembler per PID: the DVR hands back both taps interleaved, and
    // continuity counters and partial sections are per PID. A single
    // assembler fed from two streams would treat the other's packets as
    // continuity losses and discard half-built sections.
    let mut asms: Vec<TsSectionAssembler> = pids
        .iter()
        .map(|&pid| TsSectionAssembler::new(pid))
        .collect();
    let mut buf = vec![0u8; eit::TS_PACKET_SIZE * 512];

    loop {
        if Instant::now() >= deadline {
            break;
        }
        let n = match dvr.read(&mut buf) {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::debug!("dvr read: {e}");
                break;
            }
        };
        for pkt in buf[..n].chunks_exact(eit::TS_PACKET_SIZE) {
            // Each assembler ignores packets that aren't its PID, so the
            // interleaved tap output can just be offered to all of them.
            for asm in asms.iter_mut() {
                if !asm.feed(pkt) {
                    continue;
                }
                for raw in asm.drain() {
                    let table_id = raw[0];
                    let want = table_id == eit::EIT_ACTUAL_PF
                        || (include_schedule
                            && (eit::EIT_ACTUAL_SCHED_FIRST..=eit::EIT_ACTUAL_SCHED_LAST)
                                .contains(&table_id));
                    if !want {
                        continue;
                    }
                    if let Ok(sec) = eit::parse_eit_section(&raw) {
                        // Keep every service in this transport stream, not
                        // just the one we tuned for. EIT actual-TS
                        // (0x4E, 0x50-0x5F) describes *all* services of the
                        // mux, so one tune is one mux's worth of guide — a
                        // broadcaster's four services and, via PID 0x0027, its
                        // one-seg feeds. Filtering to a single service_id here
                        // was why every sibling channel had an empty schedule.
                        if only_service.is_some_and(|want| sec.service_id != want) {
                            continue;
                        }
                        log::debug!(
                            "EIT table=0x{:02x} svc={} sec={}/{} events={}",
                            sec.table_id,
                            sec.service_id,
                            sec.section_number,
                            sec.last_section_number,
                            sec.events.len()
                        );
                        // Section numbering restarts per (service, table),
                        // so the dedup key has to carry the service or one
                        // service's section 3 hides every other's.
                        if seen.insert((sec.service_id, sec.table_id, sec.section_number)) {
                            out.push(sec);
                        }
                    }
                }
            }
        }
    }
}

// ── Output formatters ────────────────────────────────────────────────────────

fn print_events_human(events: &[(u16, EitEvent)]) {
    for (service_id, ev) in events {
        let start = ev
            .start
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "??:??:??".to_string());
        let dur = ev
            .duration
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "??:??:??".to_string());
        let running = match ev.running_status {
            1 => " [not running]",
            2 => " [starts soon]",
            3 => " [pausing]",
            4 => " [running]",
            _ => "",
        };
        let genre = ev
            .content_nibbles
            .first()
            .map(|&(l1, l2)| eit::content_genre(l1, l2))
            .unwrap_or("");
        println!(
            "  svc {service_id} [{:5}]{running}  {start}  +{dur}  {}{}",
            ev.event_id,
            ev.title,
            if genre.is_empty() {
                String::new()
            } else {
                format!("  ({genre})")
            }
        );
        if !ev.text.is_empty() {
            for line in ev.text.lines() {
                println!("           {line}");
            }
        }
    }
}

/// Each event carries its own `service_id`: the harvest spans every service
/// on the mux, so the consumer must not assume they all belong to the
/// channel that was named on the command line.
fn print_events_json(events: &[(u16, EitEvent)]) {
    print!("[");
    for (i, (service_id, ev)) in events.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        let start = ev.start.as_ref().map(|t| t.to_string()).unwrap_or_default();
        let dur = ev
            .duration
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_default();
        let title = json_escape(&ev.title);
        let text = json_escape(&ev.text);
        let genres: Vec<String> = ev
            .content_nibbles
            .iter()
            .map(|&(l1, l2)| format!("\"{}\"", eit::content_genre(l1, l2)))
            .collect();
        print!(
            "{{\"service_id\":{service_id},\"event_id\":{},\"start\":\"{start}\",\
             \"duration\":\"{dur}\",\"running_status\":{},\"title\":\"{title}\",\
             \"text\":\"{text}\",\"genres\":[{}]}}",
            ev.event_id,
            ev.running_status,
            genres.join(",")
        );
    }
    println!("]");
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "")
}
