use crate::config::DvbV5Entry;
use crate::dtv_cmds::*;
use crate::error::{Error, Result};
use crate::frontend::Frontend;
use crate::sys::ffi::{
    dtv_property, fe_code_rate, fe_delivery_system, fe_guard_interval, fe_modulation,
    fe_spectral_inversion, fe_transmit_mode,
};
use crate::sys::ioctl;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const FE_HAS_LOCK: u32 = 16;

fn prop_u32(cmd: u32, data: u32) -> dtv_property {
    let mut p = dtv_property::default();
    p.cmd = cmd;
    p.u.data = data;
    p
}

fn parse_u32(m: &HashMap<String, String>, k: &str) -> Option<u32> {
    m.get(k).and_then(|s| s.parse().ok())
}

fn parse_u8(m: &HashMap<String, String>, k: &str) -> Option<u32> {
    m.get(k).and_then(|s| s.parse::<u8>().ok()).map(u32::from)
}

fn parse_inversion(s: &str) -> fe_spectral_inversion {
    match s.trim().to_uppercase().as_str() {
        "ON" | "INVERTED" => fe_spectral_inversion::INVERSION_ON,
        "OFF" | "NOT_INVERTED" => fe_spectral_inversion::INVERSION_OFF,
        _ => fe_spectral_inversion::INVERSION_AUTO,
    }
}

fn parse_guard(s: &str) -> fe_guard_interval {
    match s.trim() {
        "1/32" => fe_guard_interval::GUARD_INTERVAL_1_32,
        "1/16" => fe_guard_interval::GUARD_INTERVAL_1_16,
        "1/8" => fe_guard_interval::GUARD_INTERVAL_1_8,
        "1/4" => fe_guard_interval::GUARD_INTERVAL_1_4,
        "AUTO" | "AUTO_INTERVAL" => fe_guard_interval::GUARD_INTERVAL_AUTO,
        "1/128" => fe_guard_interval::GUARD_INTERVAL_1_128,
        _ => fe_guard_interval::GUARD_INTERVAL_AUTO,
    }
}

fn parse_transmission(s: &str) -> fe_transmit_mode {
    match s.trim().to_uppercase().as_str() {
        "2K" => fe_transmit_mode::TRANSMISSION_MODE_2K,
        "4K" => fe_transmit_mode::TRANSMISSION_MODE_4K,
        "8K" => fe_transmit_mode::TRANSMISSION_MODE_8K,
        "1K" => fe_transmit_mode::TRANSMISSION_MODE_1K,
        "16K" => fe_transmit_mode::TRANSMISSION_MODE_16K,
        "32K" => fe_transmit_mode::TRANSMISSION_MODE_32K,
        "C1" => fe_transmit_mode::TRANSMISSION_MODE_C1,
        "C3780" => fe_transmit_mode::TRANSMISSION_MODE_C3780,
        "AUTO" => fe_transmit_mode::TRANSMISSION_MODE_AUTO,
        _ => fe_transmit_mode::TRANSMISSION_MODE_AUTO,
    }
}

fn parse_delivery(s: &str) -> fe_delivery_system {
    match s.trim().to_uppercase().as_str() {
        "ISDBT" => fe_delivery_system::SYS_ISDBT,
        "DVBT" | "DVB-T" => fe_delivery_system::SYS_DVBT,
        "DVBT2" | "DVB-T2" => fe_delivery_system::SYS_DVBT2,
        "DVBC" | "DVBC_ANNEX_A" => fe_delivery_system::SYS_DVBC_ANNEX_A,
        "ATSC" => fe_delivery_system::SYS_ATSC,
        _ => fe_delivery_system::SYS_UNDEFINED,
    }
}

fn parse_modulation(s: &str) -> fe_modulation {
    let u = s.trim().to_uppercase();
    if u.contains("AUTO") {
        return fe_modulation::QAM_AUTO;
    }
    if u.contains("64") {
        return fe_modulation::QAM_64;
    }
    if u.contains("16") {
        return fe_modulation::QAM_16;
    }
    if u.contains("256") {
        return fe_modulation::QAM_256;
    }
    fe_modulation::QAM_AUTO
}

fn parse_fec(s: &str) -> fe_code_rate {
    if s.trim().eq_ignore_ascii_case("auto") {
        return fe_code_rate::FEC_AUTO;
    }
    fe_code_rate::FEC_AUTO
}

/// Build `DTV_*` chain (+ final `DTV_TUNE`) for a legacy `.conf` entry.
pub fn build_props_from_dvbv5(entry: &DvbV5Entry) -> Result<Vec<dtv_property>> {
    let m = &entry.raw;
    let mut props = Vec::with_capacity(48);

    props.push(prop_u32(DTV_CLEAR, 0));

    let delsys = parse_delivery(
        m.get("DELIVERY_SYSTEM")
            .map(String::as_str)
            .unwrap_or("ISDBT"),
    );
    if delsys == fe_delivery_system::SYS_UNDEFINED {
        return Err(Error::Msg(
            "unsupported or missing DELIVERY_SYSTEM".into(),
        ));
    }
    props.push(prop_u32(
        DTV_DELIVERY_SYSTEM,
        delsys as u32,
    ));

    let freq = parse_u32(m, "FREQUENCY").ok_or_else(|| Error::Parse("FREQUENCY".into()))?;
    props.push(prop_u32(DTV_FREQUENCY, freq));

    let bw = parse_u32(m, "BANDWIDTH_HZ").unwrap_or(entry.channel.bandwidth_hz);
    let bw = if bw > 0 { bw } else { 6_000_000 };
    props.push(prop_u32(DTV_BANDWIDTH_HZ, bw));

    if let Some(inv) = m.get("INVERSION") {
        props.push(prop_u32(
            DTV_INVERSION,
            parse_inversion(inv) as u32,
        ));
    } else {
        props.push(prop_u32(
            DTV_INVERSION,
            fe_spectral_inversion::INVERSION_AUTO as u32,
        ));
    }

    if let Some(g) = m.get("GUARD_INTERVAL") {
        props.push(prop_u32(DTV_GUARD_INTERVAL, parse_guard(g) as u32));
    }
    if let Some(t) = m.get("TRANSMISSION_MODE") {
        props.push(prop_u32(
            DTV_TRANSMISSION_MODE,
            parse_transmission(t) as u32,
        ));
    }

    macro_rules! opt_u8 {
        ($key:expr, $cmd:expr) => {
            if let Some(v) = parse_u8(m, $key) {
                props.push(prop_u32($cmd, v));
            }
        };
    }

    opt_u8!("ISDBT_LAYER_ENABLED", DTV_ISDBT_LAYER_ENABLED);
    opt_u8!(
        "ISDBT_PARTIAL_RECEPTION",
        DTV_ISDBT_PARTIAL_RECEPTION
    );
    opt_u8!(
        "ISDBT_SOUND_BROADCASTING",
        DTV_ISDBT_SOUND_BROADCASTING
    );
    opt_u8!("ISDBT_SB_SUBCHANNEL_ID", DTV_ISDBT_SB_SUBCHANNEL_ID);
    opt_u8!("ISDBT_SB_SEGMENT_IDX", DTV_ISDBT_SB_SEGMENT_IDX);
    opt_u8!(
        "ISDBT_SB_SEGMENT_COUNT",
        DTV_ISDBT_SB_SEGMENT_COUNT
    );

    if let Some(s) = m.get("ISDBT_LAYERA_FEC") {
        props.push(prop_u32(DTV_ISDBT_LAYERA_FEC, parse_fec(s) as u32));
    }
    if let Some(s) = m.get("ISDBT_LAYERA_MODULATION") {
        props.push(prop_u32(
            DTV_ISDBT_LAYERA_MODULATION,
            parse_modulation(s) as u32,
        ));
    }
    opt_u8!(
        "ISDBT_LAYERA_SEGMENT_COUNT",
        DTV_ISDBT_LAYERA_SEGMENT_COUNT
    );
    opt_u8!(
        "ISDBT_LAYERA_TIME_INTERLEAVING",
       DTV_ISDBT_LAYERA_TIME_INTERLEAVING
    );

    if let Some(s) = m.get("ISDBT_LAYERB_FEC") {
        props.push(prop_u32(DTV_ISDBT_LAYERB_FEC, parse_fec(s) as u32));
    }
    if let Some(s) = m.get("ISDBT_LAYERB_MODULATION") {
        props.push(prop_u32(
            DTV_ISDBT_LAYERB_MODULATION,
            parse_modulation(s) as u32,
        ));
    }
    opt_u8!(
        "ISDBT_LAYERB_SEGMENT_COUNT",
        DTV_ISDBT_LAYERB_SEGMENT_COUNT
    );
    opt_u8!(
        "ISDBT_LAYERB_TIME_INTERLEAVING",
        DTV_ISDBT_LAYERB_TIME_INTERLEAVING
    );

    if let Some(s) = m.get("ISDBT_LAYERC_FEC") {
        props.push(prop_u32(DTV_ISDBT_LAYERC_FEC, parse_fec(s) as u32));
    }
    if let Some(s) = m.get("ISDBT_LAYERC_MODULATION") {
        props.push(prop_u32(
            DTV_ISDBT_LAYERC_MODULATION,
            parse_modulation(s) as u32,
        ));
    }
    opt_u8!(
        "ISDBT_LAYERC_SEGMENT_COUNT",
        DTV_ISDBT_LAYERC_SEGMENT_COUNT
    );
    opt_u8!(
        "ISDBT_LAYERC_TIME_INTERLEAVING",
        DTV_ISDBT_LAYERC_TIME_INTERLEAVING
    );

    props.push(prop_u32(DTV_TUNE, 0));

    if props.len() > 64 {
        return Err(Error::Msg("property chain too long".into()));
    }

    Ok(props)
}

/// Minimal JSON/TOML-only tune (ISDB-T / DVB-T style).
pub fn build_props_simple_isdbt(
    frequency: u32,
    bandwidth_hz: u32,
    _service_id: u16,
) -> Vec<dtv_property> {
    let bw = if bandwidth_hz > 0 { bandwidth_hz } else { 6_000_000 };
    vec![
        prop_u32(DTV_CLEAR, 0),
        prop_u32(DTV_DELIVERY_SYSTEM, fe_delivery_system::SYS_ISDBT as u32),
        prop_u32(DTV_FREQUENCY, frequency),
        prop_u32(DTV_BANDWIDTH_HZ, bw),
        prop_u32(
            DTV_INVERSION,
            fe_spectral_inversion::INVERSION_AUTO as u32,
        ),
        prop_u32(
            DTV_GUARD_INTERVAL,
            fe_guard_interval::GUARD_INTERVAL_AUTO as u32,
        ),
        prop_u32(
            DTV_TRANSMISSION_MODE,
            fe_transmit_mode::TRANSMISSION_MODE_AUTO as u32,
        ),
        prop_u32(DTV_TUNE, 0),
    ]
}

pub fn tune_frontend(fe: &Frontend, entry: &DvbV5Entry) -> Result<()> {
    let mut props = build_props_from_dvbv5(entry)?;
    fe.set_properties(&mut props)?;
    Ok(())
}

pub fn wait_lock(
    fe: &Frontend,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let mut mask = 0u32;
        unsafe {
            ioctl::fe_read_lock_status(fe.as_raw_fd(), &mut mask)?;
        }
        if mask & FE_HAS_LOCK != 0 {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(Error::LockTimeout);
        }
        std::thread::sleep(poll);
    }
}
