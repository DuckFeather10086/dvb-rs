use clap::{Parser, Subcommand};
use dvbr::channel::ChannelsFile;
use dvbr::config;
use dvbr::demux::{Demux, DvrReader};
use dvbr::frontend::Frontend;
use dvbr::scan;
use dvbr::signal;
use dvbr::sys::ffi;
use dvbr::tuner::{tune_frontend, wait_lock};
use log::info;
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const BUF: usize = 188 * 512;

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
}

fn main() -> dvbr::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Tune {
            adapter,
            frontend,
            demux,
            channels,
            output,
            lock_timeout_ms,
            name,
        } => {
            let entries = config::load_channel_entries(&channels)?;
            let entry = config::find_entry(&entries, &name)?;
            let fe = Frontend::open_rw(adapter, frontend)?;
            tune_frontend(&fe, entry)?;
            wait_lock(
                &fe,
                Duration::from_millis(lock_timeout_ms),
                Duration::from_millis(100),
            )?;
            info!("locked; starting demux full mux (PID 0x2000)");

            let dm = Demux::open_rw(adapter, demux)?;
            dm.zap_all_pids_to_dvr()?;
            let mut dvr = DvrReader::open_ro(adapter, demux)?;
            let mut buf = vec![0u8; BUF];

            let mut out: Box<dyn Write> = if output == "-" {
                Box::new(io::stdout().lock())
            } else {
                Box::new(File::create(&output)?)
            };

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
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
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
        } => {
            let (entry, freq, bw) = if let Some(path) = channels {
                let entries = config::load_channel_entries(&path)?;
                let n = name
                    .ok_or_else(|| dvbr::Error::Msg("--name required with --channels".into()))?;
                let e = config::find_entry(&entries, &n)?;
                (
                    Some(e.clone()),
                    e.channel.frequency,
                    e.channel.bandwidth_hz,
                )
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
            config::write_channels_json(&output, &ch)?;
            info!("wrote {} channel(s) to {}", ch.channels.len(), output.display());
        }
        Cmd::FeInfo { adapter, frontend } => {
            let fe = Frontend::open_rw(adapter, frontend)?;
            let mut info = ffi::dvb_frontend_info::default();
            fe.get_frontend_info(&mut info)?;
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }
                .to_string_lossy();
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
            if output.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase() == "toml"
            {
                let s = toml::to_string_pretty(&doc)
                    .map_err(|e| dvbr::Error::Parse(e.to_string()))?;
                std::fs::write(&output, s)?;
            } else {
                config::write_channels_document_json(&output, &doc)?;
            }
            info!("migrated {} channels -> {}", n, output.display());
        }
    }
    Ok(())
}
