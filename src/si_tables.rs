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

/// Best-effort SDT actual parser. Service names come from the service
/// descriptor (`0x48`) and are decoded as ARIB STD-B24 text (same as
/// EIT event names in `eit.rs`) — not raw UTF-8.
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

    // SDT has no service-loop-length field (unlike PMT's program_info_length):
    // the loop starts right after the fixed header at byte 11 and runs to the
    // end of the section, less the 4-byte CRC.
    //
    //   [0]      table_id
    //   [1..3]   syntax/section_length
    //   [3..5]   transport_stream_id
    //   [5]      version / current_next
    //   [6]      section_number
    //   [7]      last_section_number
    //   [8..10]  original_network_id
    //   [10]     reserved_future_use
    //   [11..]   service loop
    let mut off = 11usize;
    let end_loop = tot - 4;

    let mut services = Vec::new();
    // Each entry: service_id(2) + EIT flags(1) + running/CA/desc_len(2) = 5.
    while off + 5 <= end_loop {
        let sid = u16::from(section[off]) << 8 | u16::from(section[off + 1]);
        let dlen =
            ((u16::from(section[off + 3]) << 8 | u16::from(section[off + 4])) & 0x0fff) as usize;
        off += 5;
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
                        name = arib_b24::decode(raw);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One service-loop entry, framed per EN 300 468 §5.2.3:
    /// service_id(2), EIT flags(1), running_status/free_CA/desc_len(2).
    fn sdt_service_entry(service_id: u16, name_bytes: &[u8]) -> Vec<u8> {
        // 0x48 descriptor body: service_type, provider_len=0, name_len, name.
        let mut desc_body = vec![0x01u8, 0x00, name_bytes.len() as u8];
        desc_body.extend_from_slice(name_bytes);
        let descriptor = {
            let mut d = vec![0x48u8, desc_body.len() as u8];
            d.extend_from_slice(&desc_body);
            d
        };
        let mut svc = service_id.to_be_bytes().to_vec();
        svc.push(0x00); // reserved(6) + EIT_schedule + EIT_present_following
                        // running_status(3) | free_CA(1) | descriptors_loop_length(12)
        svc.push(0x80 | ((descriptor.len() >> 8) as u8 & 0x0f));
        svc.push(descriptor.len() as u8);
        svc.extend_from_slice(&descriptor);
        svc
    }

    // Build a real-layout SDT-actual section from pre-framed service
    // entries: fixed 11-byte header, then the service loop straight to the
    // CRC — SDT has no loop-length field. The CRC is computed in-place.
    fn sdt_section(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut sec = vec![
            SDT_ACTUAL_TABLE_ID, // [0]
            0x00,
            0x00, // [1..3] section_length, filled below
            0x00,
            0x01, // [3..5] transport_stream_id
            0x01, // [5]  version / current_next
            0x00, // [6]  section_number
            0x00, // [7]  last_section_number
            0x00,
            0x00, // [8..10] original_network_id
            0x00, // [10] reserved_future_use
        ];
        for e in entries {
            sec.extend_from_slice(e); // [11..] service loop
        }

        // section_length = bytes after [2], including the 4-byte CRC.
        let section_length = (sec.len() - 3 + 4) as u16;
        sec[1] = 0xb0 | ((section_length >> 8) as u8 & 0x0f);
        sec[2] = section_length as u8;

        let crc = crc32_mpeg(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        sec
    }

    #[test]
    fn sdt_service_name_is_arib_b24_decoded() {
        // ESC $ 3B; LS0; GL 0x7A 0x56 → ARIB additional symbol U+1F211 (🈑).
        // from_utf8_lossy would yield the literal control bytes, not 🈑.
        let arib = [0x1b, 0x24, 0x3b, 0x0f, 0x7a, 0x56];
        let section = sdt_section(&[sdt_service_entry(1234, &arib)]);

        let services = parse_sdt(&section).expect("parse SDT");
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].service_id, 1234);
        assert_eq!(
            services[0].name, "🈑",
            "service name must be ARIB-B24 decoded (got {:?})",
            services[0].name
        );
        assert_ne!(
            services[0].name,
            String::from_utf8_lossy(&arib),
            "name must not be the raw from_utf8_lossy fallback",
        );
    }

    // Regression: the loop framing used to be read as PMT-style
    // (a 12-bit loop length at [11..13], 4-byte service entries). Real SDT
    // has neither — the loop starts at [11] and each entry header is 5
    // bytes. With the old framing a real broadcast SDT failed outright
    // ("bad SDT service loop length"), so every channel fell back to a
    // `program_<n>` placeholder name.
    #[test]
    fn sdt_parses_every_service_in_the_loop() {
        let arib = [0x1b, 0x24, 0x3b, 0x0f, 0x7a, 0x56];
        let section = sdt_section(&[
            sdt_service_entry(1064, &arib),
            sdt_service_entry(1065, &[]), // present, but unnamed
            sdt_service_entry(1448, &arib),
        ]);

        let services = parse_sdt(&section).expect("parse SDT");
        let ids: Vec<u16> = services.iter().map(|s| s.service_id).collect();
        assert_eq!(
            ids,
            vec![1064, 1065, 1448],
            "every service in the loop must be reported, in order"
        );
        assert_eq!(services[0].name, "🈑");
        assert_eq!(services[1].name, "", "unnamed service yields an empty name");
        assert_eq!(services[2].name, "🈑");
    }

    // A section whose final entry is cut short must yield the entries that
    // are intact rather than erroring the whole table away.
    #[test]
    fn sdt_tolerates_a_truncated_trailing_entry() {
        let arib = [0x1b, 0x24, 0x3b, 0x0f, 0x7a, 0x56];
        let mut good = sdt_service_entry(1064, &arib);
        good.extend_from_slice(&[0x04, 0x29, 0x00]); // 3 bytes of a 5-byte header
        let section = sdt_section(&[good]);

        let services = parse_sdt(&section).expect("parse SDT");
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].service_id, 1064);
    }
}
