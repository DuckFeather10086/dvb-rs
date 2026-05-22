use crate::channel::{Channel, ChannelsFile};
use crate::config::DvbV5Entry;
use crate::demux::Demux;
use crate::error::{Error, Result};
use crate::frontend::Frontend;
use crate::si_tables::{parse_pat, parse_sdt, section_byte_len_from_prefix};
use crate::sys::dmx_abi::{dmx_stream_stop, DmxSctFilterParams};
use crate::tuner::{build_props_simple_isdbt, tune_frontend, wait_lock};
use std::time::Duration;

fn read_one_section(dm: &mut Demux, buf: &mut Vec<u8>, scratch: &mut [u8]) -> Result<usize> {
    buf.clear();
    while buf.len() < 3 {
        let n = dm.read(scratch).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::Si("demux closed before section header".into())
            } else {
                Error::Io(e)
            }
        })?;
        if n == 0 {
            return Err(Error::Si("short read on demux".into()));
        }
        buf.extend_from_slice(&scratch[..n]);
    }
    let tot =
        section_byte_len_from_prefix(&buf[..3]).ok_or_else(|| Error::Si("bad SI header".into()))?;
    if tot > 4096 {
        return Err(Error::Si("section too large".into()));
    }
    while buf.len() < tot {
        let n = dm.read(scratch).map_err(|e: std::io::Error| Error::Io(e))?;
        if n == 0 {
            return Err(Error::Si("short read on demux".into()));
        }
        buf.extend_from_slice(&scratch[..n]);
    }
    Ok(tot)
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

    let mut dm = Demux::open_rw(adapter, dmx_id)?;
    let mut buf = Vec::with_capacity(1024);
    let mut scratch = [0u8; 4096];

    let p_pat = DmxSctFilterParams::pat(0);
    dm.set_section_filter(&p_pat)?;
    let _n = read_one_section(&mut dm, &mut buf, &mut scratch)?;
    let pat = parse_pat(&buf[.._n])?;
    unsafe {
        let _ = dmx_stream_stop(dm.as_raw_fd());
    }

    let p_sdt = DmxSctFilterParams::sdt_actual(0x0011);
    dm.set_section_filter(&p_sdt)?;
    let n2 = read_one_section(&mut dm, &mut buf, &mut scratch)?;
    let sdt = parse_sdt(&buf[..n2])?;

    unsafe {
        let _ = dmx_stream_stop(dm.as_raw_fd());
    }

    let mut channels = Vec::new();
    for s in sdt {
        let mut name = s.name;
        if name.is_empty() {
            name = format!("service_{}", s.service_id);
        }
        let ch = Channel {
            name,
            delivery: del.clone(),
            frequency: tune_freq,
            bandwidth_hz: tune_bw,
            service_id: s.service_id,
            video_pids: Vec::new(),
            audio_pids: Vec::new(),
        };
        channels.push(ch);
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
