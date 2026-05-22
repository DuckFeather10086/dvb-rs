//! PAT / SDT section parsing (MPEG-2 TS SI).

use crate::error::{Error, Result};

const PAT_TABLE_ID: u8 = 0x00;
const PMT_TABLE_ID: u8 = 0x02;
const SDT_ACTUAL_TABLE_ID: u8 = 0x42;

/// `true` if DVB CRC-32 over `data` equals crc stored in last 4 bytes (big-endian).
pub fn section_crc_ok(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let n = data.len() - 4;
    let expected = u32::from(data[n]) << 24
        | u32::from(data[n + 1]) << 16
        | u32::from(data[n + 2]) << 8
        | u32::from(data[n + 3]);
    let crc = crc32_mpeg(&data[..n]);
    crc == expected
}

/// CRC-32 per EN 300 468 / MPEG-2 SI (poly `0x04C11DB7`, init `0xFFFFFFFF`, no final xor).
fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    const POLY: u32 = 0x04c1_1db7;
    for &b in data {
        crc ^= (u32::from(b)) << 24;
        for _ in 0..8 {
            if crc & 0x8000_0000 != 0 {
                crc = (crc << 1) ^ POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Total section byte length (including CRC) from the first 3 bytes.
pub fn section_byte_len_from_prefix(header3: &[u8]) -> Option<usize> {
    if header3.len() < 3 {
        return None;
    }
    let len12 = u16::from(header3[1]) << 8 | u16::from(header3[2]);
    let slen = (len12 & 0x0fff) as usize;
    Some(3 + slen)
}

fn section_total_len(header3: &[u8]) -> Option<usize> {
    section_byte_len_from_prefix(header3)
}

#[derive(Debug, Clone)]
pub struct PatProgram {
    pub program_number: u16,
    pub pid: u16,
}

pub fn parse_pat(section: &[u8]) -> Result<Vec<PatProgram>> {
    if section.len() < 16 {
        return Err(Error::Si("PAT too short".into()));
    }
    if section[0] != PAT_TABLE_ID {
        return Err(Error::Si("not a PAT section".into()));
    }
    if !section_crc_ok(section) {
        return Err(Error::Si("PAT CRC mismatch".into()));
    }
    let tot = section_total_len(
        <&[u8; 3]>::try_from(&section[..3]).map_err(|_| Error::Si("PAT header".into()))?,
    )
    .ok_or_else(|| Error::Si("bad PAT header".into()))?;
    if section.len() < tot {
        return Err(Error::Si("incomplete PAT section".into()));
    }
    let mut off = 8usize;
    let mut out = Vec::new();
    while off + 4 <= tot - 4 {
        let pn = u16::from(section[off]) << 8 | u16::from(section[off + 1]);
        let pid = (u16::from(section[off + 2]) & 0x1f) << 8 | u16::from(section[off + 3]);
        off += 4;
        out.push(PatProgram {
            program_number: pn,
            pid,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct PmtStream {
    pub stream_type: u8,
    pub pid: u16,
}

#[derive(Debug, Clone)]
pub struct Pmt {
    pub program_number: u16,
    pub pcr_pid: u16,
    pub ca_pids: Vec<u16>,
    pub streams: Vec<PmtStream>,
}

fn collect_ca_pids(descriptors: &[u8], out: &mut Vec<u16>) {
    let mut off = 0usize;
    while off + 2 <= descriptors.len() {
        let tag = descriptors[off];
        let len = descriptors[off + 1] as usize;
        off += 2;
        if off + len > descriptors.len() {
            break;
        }
        let d = &descriptors[off..off + len];
        if tag == 0x09 && d.len() >= 4 {
            let pid = (u16::from(d[2]) & 0x1f) << 8 | u16::from(d[3]);
            out.push(pid);
        }
        off += len;
    }
}

pub fn parse_pmt(section: &[u8]) -> Result<Pmt> {
    if section.len() < 16 {
        return Err(Error::Si("PMT too short".into()));
    }
    if section[0] != PMT_TABLE_ID {
        return Err(Error::Si("not a PMT section".into()));
    }
    if !section_crc_ok(section) {
        return Err(Error::Si("PMT CRC mismatch".into()));
    }
    let tot = section_total_len(
        <&[u8; 3]>::try_from(&section[..3]).map_err(|_| Error::Si("PMT header".into()))?,
    )
    .ok_or_else(|| Error::Si("bad PMT header".into()))?;
    if section.len() < tot {
        return Err(Error::Si("incomplete PMT section".into()));
    }

    let program_number = u16::from(section[3]) << 8 | u16::from(section[4]);
    let pcr_pid = (u16::from(section[8]) & 0x1f) << 8 | u16::from(section[9]);
    let program_info_len = ((usize::from(section[10]) & 0x0f) << 8) | usize::from(section[11]);
    let end = tot.saturating_sub(4);
    let program_info_start = 12usize;
    let program_info_end = program_info_start.saturating_add(program_info_len);
    if program_info_end > end {
        return Err(Error::Si("bad PMT program info length".into()));
    }

    let mut ca_pids = Vec::new();
    collect_ca_pids(&section[program_info_start..program_info_end], &mut ca_pids);

    let mut off = program_info_end;
    if off > end {
        return Err(Error::Si("bad PMT program info length".into()));
    }

    let mut streams = Vec::new();
    while off + 5 <= end {
        let stream_type = section[off];
        let pid = (u16::from(section[off + 1]) & 0x1f) << 8 | u16::from(section[off + 2]);
        let es_info_len =
            ((usize::from(section[off + 3]) & 0x0f) << 8) | usize::from(section[off + 4]);
        let es_info_start = off + 5;
        let es_info_end = es_info_start.saturating_add(es_info_len);
        if es_info_end > end {
            return Err(Error::Si("bad PMT ES info length".into()));
        }
        collect_ca_pids(&section[es_info_start..es_info_end], &mut ca_pids);
        streams.push(PmtStream { stream_type, pid });
        off = es_info_end;
    }
    ca_pids.sort_unstable();
    ca_pids.dedup();

    Ok(Pmt {
        program_number,
        pcr_pid,
        ca_pids,
        streams,
    })
}

#[derive(Debug, Clone)]
pub struct SdtService {
    pub service_id: u16,
    pub name: String,
}

/// Best-effort SDT actual parser (service names via service descriptor `0x48`).
pub fn parse_sdt(section: &[u8]) -> Result<Vec<SdtService>> {
    if section.len() < 15 {
        return Err(Error::Si("SDT too short".into()));
    }
    if section[0] != SDT_ACTUAL_TABLE_ID {
        return Err(Error::Si("not an SDT actual section".into()));
    }
    if !section_crc_ok(section) {
        return Err(Error::Si("SDT CRC mismatch".into()));
    }
    let tot = section_total_len(
        <&[u8; 3]>::try_from(&section[..3]).map_err(|_| Error::Si("SDT header".into()))?,
    )
    .ok_or_else(|| Error::Si("bad SDT header".into()))?;
    if section.len() < tot {
        return Err(Error::Si("incomplete SDT section".into()));
    }

    let loop_len = ((usize::from(section[11]) & 0x0f) << 8) | usize::from(section[12]);
    let mut off = 13usize;
    let end_loop = off.saturating_add(loop_len);
    if end_loop + 4 > tot {
        return Err(Error::Si("bad SDT service loop length".into()));
    }

    let mut services = Vec::new();
    while off + 4 <= end_loop {
        let sid = u16::from(section[off]) << 8 | u16::from(section[off + 1]);
        let dlen =
            ((u16::from(section[off + 2]) << 8 | u16::from(section[off + 3])) & 0x0fff) as usize;
        off += 4;
        let desc_end = off.saturating_add(dlen);
        if desc_end > end_loop {
            break;
        }
        let mut name = String::new();
        let mut d = off;
        while d + 2 <= desc_end {
            let tag = section[d];
            let dl = section[d + 1] as usize;
            d += 2;
            if d + dl > desc_end {
                break;
            }
            if tag == 0x48 && dl >= 2 {
                if d + 2 > desc_end {
                    d += dl;
                    continue;
                }
                let provider_len = section[d + 1] as usize;
                let mut nstart = d + 2 + provider_len;
                if nstart < d + dl {
                    let name_len = section[nstart] as usize;
                    nstart += 1;
                    if nstart + name_len <= d + dl {
                        let raw = &section[nstart..nstart + name_len];
                        name = decode_dvb_text(raw);
                    }
                }
            }
            d += dl;
        }
        services.push(SdtService {
            service_id: sid,
            name,
        });
        off = desc_end;
    }
    Ok(services)
}

fn decode_dvb_text(raw: &[u8]) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if raw[0] == 0xfe {
        return String::from_utf8_lossy(&raw[1..]).into_owned();
    }
    if raw[0] <= 0x1f && raw.len() > 1 {
        return String::from_utf8_lossy(&raw[1..]).into_owned();
    }
    String::from_utf8_lossy(raw).into_owned()
}
