use clap::{Parser, Subcommand};
use dvbr::channel::ChannelsFile;
use dvbr::config;
use dvbr::demux::{Demux, DvrReader, PidTaps};
use dvbr::eit::TsSectionAssembler;
use dvbr::eit::{self, EitEvent, EitSection};
use dvbr::frontend::Frontend;
use dvbr::scan;
use dvbr::si_reader;
use dvbr::si_tables::{parse_pat, parse_pmt};
use dvbr::signal;
use dvbr::sys::ffi;
use dvbr::tuner::{tune_frontend, wait_lock};
use log::{info, warn};
use std::collections::HashMap;
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
#[command(name = "dvbr")]
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
        /// Channel name (alias or dvbr_name in channels.json)
        name: String,
    },
}

struct AdapterLock {
    _file: File,
}

fn acquire_adapter_lock(adapter: u32) -> dvbr::Result<Option<AdapterLock>> {
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
        Err(dvbr::Error::Msg(format!(
            "DVB adapter {adapter} is already in use (lock: {path})"
        )))
    } else {
        Err(err.into())
    }
}

fn service_pids_from_pat_pmt(adapter: u32, demux: u32, service_id: u16) -> dvbr::Result<Vec<u16>> {
    let pat_section = si_reader::read_section(adapter, demux, 0x0000, 0x00, SI_PID_READ_TIMEOUT)?;
    let pat = parse_pat(&pat_section)?;
    let pmt_pid = pat
        .iter()
        .find(|program| program.program_number == service_id)
        .map(|program| program.pid)
        .ok_or_else(|| dvbr::Error::Si(format!("service_id {service_id} not found in PAT")))?;

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

fn bounded_epg_collect_secs(schedule: bool, collect_secs: Option<u64>) -> dvbr::Result<u64> {
    let secs = collect_secs.unwrap_or(if schedule { 60 } else { 15 });
    if secs == 0 {
        return Err(dvbr::Error::Msg("--collect-secs must be at least 1".into()));
    }
    if secs > MAX_EPG_COLLECT_SECS {
        warn!("capping EPG collection to {MAX_EPG_COLLECT_SECS}s (requested {secs}s)");
        return Ok(MAX_EPG_COLLECT_SECS);
    }
    Ok(secs)
}

fn main() -> dvbr::Result<()> {
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
        } => {
            let _lock = acquire_adapter_lock(adapter)?;
            let (entry, freq, bw) = if let Some(path) = channels {
                let entries = config::load_channel_entries(&path)?;
                let n =
                    name.ok_or_else(|| dvbr::Error::Msg("--name required with --channels".into()))?;
                let e = config::find_entry(&entries, &n)?;
                (Some(e.clone()), e.channel.frequency, e.channel.bandwidth_hz)
            } else {
                let f = frequency.ok_or_else(|| {
                    dvbr::Error::Msg("either --channels + --name or --frequency is required".into())
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
                let report = scan::merge_scanned_names(&mut doc, &ch);
                config::write_channels_document_json(&output, &doc)?;
                for (from, to) in &report.renamed {
                    info!("renamed {from} -> {to}");
                }
                for name in &report.aliased {
                    info!("added broadcast name as alias on {name}");
                }
                if !report.unmatched.is_empty() {
                    info!(
                        "scanned service(s) with no matching record: {:?}",
                        report.unmatched
                    );
                }
                info!(
                    "merged {} scanned service(s) into {} ({} renamed, {} aliased)",
                    ch.channels.len(),
                    output.display(),
                    report.renamed.len(),
                    report.aliased.len()
                );
            } else {
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
                    toml::to_string_pretty(&doc).map_err(|e| dvbr::Error::Parse(e.to_string()))?;
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
            // EIT on PID 0x0012. Use a PES tap filter instead: this routes only PID 0x0012 TS
            // packets to the DVR device, reducing USB bandwidth and packet loss compared to the
            // full-mux approach, while we reassemble sections in userspace.
            let dm = Demux::open_rw(adapter, demux)?;
            unsafe {
                use dvbr::sys::dmx_abi::{
                    dmx_apply_pes_filter, dmx_stream_start, DmxPesFilterParams,
                };
                let p = DmxPesFilterParams::ts_tap_pid(eit::EIT_PID);
                dmx_apply_pes_filter(dm.as_raw_fd(), &p)?;
                dmx_stream_start(dm.as_raw_fd())?;
            }
            let mut dvr = DvrReader::open_ro(adapter, demux)?;

            let mut all_sections: Vec<EitSection> = Vec::new();
            let mut seen: HashMap<(u8, u8), bool> = HashMap::new();

            info!(
                "collecting EIT for {}s (PID 0x0012 PES tap, userspace assembly)...",
                secs
            );
            collect_eit_from_dvr(
                &mut dvr,
                service_id,
                schedule,
                deadline,
                &mut all_sections,
                &mut seen,
            );

            unsafe {
                let _ = dvbr::sys::dmx_abi::dmx_stream_stop(dm.as_raw_fd());
            }

            // Gather all events, deduplicate by event_id (keep the richest copy: same id is
            // often sent as TBC placeholder first, then filled in later sections / repetitions).
            let mut events: HashMap<u16, EitEvent> = HashMap::new();
            for sec in &all_sections {
                for ev in &sec.events {
                    let score = |e: &EitEvent| {
                        e.title.chars().count()
                            + e.text.chars().count()
                            + e.content_nibbles.len().saturating_mul(8)
                    };
                    events
                        .entry(ev.event_id)
                        .and_modify(|existing| {
                            if score(ev) > score(existing) {
                                *existing = ev.clone();
                            }
                        })
                        .or_insert_with(|| ev.clone());
                }
            }
            let mut sorted: Vec<EitEvent> = events.into_values().collect();
            sorted.sort_by(|a, b| a.start.cmp(&b.start));
            if !include_empty {
                sorted.retain(|ev| !ev.title.trim().is_empty() || !ev.text.trim().is_empty());
            }

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

fn collect_eit_from_dvr(
    dvr: &mut DvrReader,
    service_id: u16,
    include_schedule: bool,
    deadline: Instant,
    out: &mut Vec<EitSection>,
    seen: &mut HashMap<(u8, u8), bool>,
) {
    let mut asm = TsSectionAssembler::new(eit::EIT_PID);
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
                match eit::parse_eit_section(&raw) {
                    Ok(sec) if sec.service_id == service_id => {
                        log::debug!(
                            "EIT table=0x{:02x} svc={} sec={}/{} events={}",
                            sec.table_id,
                            sec.service_id,
                            sec.section_number,
                            sec.last_section_number,
                            sec.events.len()
                        );
                        let key = (sec.table_id, sec.section_number);
                        if !seen.contains_key(&key) {
                            seen.insert(key, true);
                            out.push(sec);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }
}

// ── Output formatters ────────────────────────────────────────────────────────

fn print_events_human(events: &[EitEvent]) {
    for ev in events {
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
            "  [{:5}]{running}  {start}  +{dur}  {}{}",
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

fn print_events_json(events: &[EitEvent]) {
    print!("[");
    for (i, ev) in events.iter().enumerate() {
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
            "{{\"event_id\":{},\"start\":\"{start}\",\"duration\":\"{dur}\",\
             \"running_status\":{},\"title\":\"{title}\",\"text\":\"{text}\",\
             \"genres\":[{}]}}",
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
