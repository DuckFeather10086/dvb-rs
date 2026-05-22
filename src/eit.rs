//! EIT (Event Information Table) parser for ISDB-T / ARIB STD-B10.
//! Parses EIT sections from PID 0x0012 and decodes ARIB STD-B24 text.

use crate::error::{Error, Result};
use crate::si_tables::section_crc_ok;

pub const EIT_PID: u16 = 0x0012;

/// EIT table_id values.
pub const EIT_ACTUAL_PF: u8 = 0x4e; // present/following, this TS
pub const EIT_OTHER_PF: u8 = 0x4f; // present/following, other TS
pub const EIT_ACTUAL_SCHED_FIRST: u8 = 0x50;
pub const EIT_ACTUAL_SCHED_LAST: u8 = 0x5f;

#[derive(Debug, Clone)]
pub struct EitSection {
    pub table_id: u8,
    pub service_id: u16,
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub section_number: u8,
    pub last_section_number: u8,
    pub segment_last_section_number: u8,
    pub last_table_id: u8,
    pub events: Vec<EitEvent>,
}

#[derive(Debug, Clone)]
pub struct EitEvent {
    pub event_id: u16,
    pub start: Option<DateTime>,
    pub duration: Option<HmsDuration>,
    pub running_status: u8,
    pub free_ca: bool,
    pub title: String,
    pub text: String,
    pub content_nibbles: Vec<(u8, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

impl std::fmt::Display for DateTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        )
    }
}

#[derive(Debug, Clone)]
pub struct HmsDuration {
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

impl std::fmt::Display for HmsDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}:{:02}", self.hour, self.min, self.sec)
    }
}

// ── Section parser ───────────────────────────────────────────────────────────

pub fn parse_eit_section(section: &[u8]) -> Result<EitSection> {
    if section.len() < 18 {
        return Err(Error::Si("EIT section too short".into()));
    }
    let table_id = section[0];
    if !matches!(table_id, 0x4e..=0x6f) {
        return Err(Error::Si(format!("not an EIT table_id: 0x{table_id:02x}")));
    }
    if !section_crc_ok(section) {
        return Err(Error::Si("EIT CRC mismatch".into()));
    }

    let section_len = ((usize::from(section[1]) & 0x0f) << 8) | usize::from(section[2]);
    let total = 3 + section_len;
    if section.len() < total {
        return Err(Error::Si("EIT section truncated".into()));
    }

    let service_id = u16::from(section[3]) << 8 | u16::from(section[4]);
    let section_number = section[6];
    let last_section_number = section[7];
    let transport_stream_id = u16::from(section[8]) << 8 | u16::from(section[9]);
    let original_network_id = u16::from(section[10]) << 8 | u16::from(section[11]);
    let segment_last_section_number = section[12];
    let last_table_id = section[13];

    let mut events = Vec::new();
    let mut off = 14usize;
    let end = total - 4; // exclude CRC

    while off + 12 <= end {
        let event_id = u16::from(section[off]) << 8 | u16::from(section[off + 1]);
        let start = parse_datetime(&section[off + 2..off + 7]);
        let duration = parse_duration(&section[off + 7..off + 10]);
        let running_status = section[off + 10] >> 5;
        let free_ca = (section[off + 10] >> 4) & 1 == 1;
        let desc_len =
            (usize::from(section[off + 10]) & 0x0f) << 8 | usize::from(section[off + 11]);
        off += 12;

        let desc_end = off + desc_len;
        if desc_end > end {
            break;
        }

        let (title, text, content_nibbles) = parse_event_descriptors(&section[off..desc_end]);
        off = desc_end;

        events.push(EitEvent {
            event_id,
            start,
            duration,
            running_status,
            free_ca,
            title,
            text,
            content_nibbles,
        });
    }

    Ok(EitSection {
        table_id,
        service_id,
        transport_stream_id,
        original_network_id,
        section_number,
        last_section_number,
        segment_last_section_number,
        last_table_id,
        events,
    })
}

// ── Descriptor parsing ───────────────────────────────────────────────────────

fn parse_event_descriptors(data: &[u8]) -> (String, String, Vec<(u8, u8)>) {
    let mut title = String::new();
    let mut text = String::new();
    let mut content_nibbles = Vec::new();
    let mut off = 0usize;

    while off + 2 <= data.len() {
        let tag = data[off];
        let dlen = data[off + 1] as usize;
        off += 2;
        if off + dlen > data.len() {
            break;
        }
        let d = &data[off..off + dlen];

        match tag {
            0x4d => {
                // short_event_descriptor (may appear multiple times, e.g. languages / TBC slots)
                if d.len() >= 4 {
                    let name_len = d[3] as usize;
                    let name_end = 4 + name_len;
                    if name_end <= d.len() {
                        let new_title = arib_b24::decode(&d[4..name_end]);
                        let mut new_text = String::new();
                        let text_off = name_end;
                        if text_off + 1 <= d.len() {
                            let text_len = d[text_off] as usize;
                            let text_end = text_off + 1 + text_len;
                            if text_end <= d.len() {
                                new_text = arib_b24::decode(&d[text_off + 1..text_end]);
                            }
                        }
                        // Do not let a later empty descriptor wipe a good title/text.
                        if new_title.len() > title.len() {
                            title = new_title;
                        }
                        if new_text.len() > text.len() {
                            text = new_text;
                        }
                    }
                }
            }
            0x4e => {
                // extended_event_descriptor – append extra text
                if d.len() >= 5 {
                    let item_len = d[4] as usize;
                    let text_off = 5 + item_len;
                    if text_off + 1 <= d.len() {
                        let tlen = d[text_off] as usize;
                        let tend = text_off + 1 + tlen;
                        if tend <= d.len() {
                            let ext = arib_b24::decode(&d[text_off + 1..tend]);
                            if !ext.is_empty() {
                                if !text.is_empty() {
                                    text.push(' ');
                                }
                                text.push_str(&ext);
                            }
                        }
                    }
                }
            }
            0x54 => {
                // content_descriptor: pairs of nibbles (level1, level2)
                let mut i = 0;
                while i + 2 <= d.len() {
                    let l1 = d[i] >> 4;
                    let l2 = d[i] & 0x0f;
                    content_nibbles.push((l1, l2));
                    i += 2;
                }
            }
            _ => {}
        }

        off += dlen;
    }

    (title, text, content_nibbles)
}

// ── Time helpers ─────────────────────────────────────────────────────────────

/// Parse 5-byte MJD+BCD (HHMMSS) start time; returns None for all-0xFF (undefined).
pub fn parse_datetime(b: &[u8]) -> Option<DateTime> {
    if b.len() < 5 {
        return None;
    }
    if b[0] == 0xff && b[1] == 0xff {
        return None;
    }
    let mjd = u32::from(b[0]) << 8 | u32::from(b[1]);
    let hour = bcd2(b[2])?;
    let min = bcd2(b[3])?;
    let sec = bcd2(b[4])?;

    // MJD → Gregorian (ETSI EN 300 468 Annex C)
    let mjd = mjd as i64;
    let y_prime = ((mjd - 15078) * 10000 / 3652425) as i64;
    let m_prime = ((mjd - 14956 - y_prime * 36525 / 100) * 10000 / 306001) as i64;
    let d = mjd - 14956 - y_prime * 36525 / 100 - m_prime * 306001 / 10000;
    let k: i64 = if m_prime == 14 || m_prime == 15 { 1 } else { 0 };
    let year = y_prime + 1900 + k;
    let month = m_prime - 1 - k * 12;

    Some(DateTime {
        year: year as u16,
        month: month as u8,
        day: d as u8,
        hour,
        min,
        sec,
    })
}

/// Parse 3-byte BCD HHMMSS duration.
pub fn parse_duration(b: &[u8]) -> Option<HmsDuration> {
    if b.len() < 3 {
        return None;
    }
    if b[0] == 0xff {
        return None;
    }
    Some(HmsDuration {
        hour: bcd2(b[0])?,
        min: bcd2(b[1])?,
        sec: bcd2(b[2])?,
    })
}

fn bcd2(b: u8) -> Option<u8> {
    let hi = b >> 4;
    let lo = b & 0x0f;
    if hi > 9 || lo > 9 {
        None
    } else {
        Some(hi * 10 + lo)
    }
}

// ── TS section assembler ─────────────────────────────────────────────────────

pub const TS_PACKET_SIZE: usize = 188;

/// Reassembles SI sections from raw 188-byte TS packets for a given PID.
/// Tracks the continuity counter (CC) and discards any partial section when a CC
/// gap is detected, preventing corrupted bytes from reaching CRC validation.
pub struct TsSectionAssembler {
    pid: u16,
    partial: Vec<u8>,
    ready: Vec<Vec<u8>>,
    last_cc: Option<u8>,
    partial_valid: bool, // false if a CC gap was seen during current section
    cc_drops: u64,
}

impl TsSectionAssembler {
    pub fn new(pid: u16) -> Self {
        Self {
            pid,
            partial: Vec::with_capacity(4096),
            ready: Vec::new(),
            last_cc: None,
            partial_valid: false,
            cc_drops: 0,
        }
    }

    /// Feed one 188-byte TS packet. Returns true if at least one complete section
    /// is now available via [`TsSectionAssembler::drain`].
    pub fn feed(&mut self, pkt: &[u8]) -> bool {
        if pkt.len() < 188 || pkt[0] != 0x47 {
            return false;
        }
        let pid = ((pkt[1] as u16 & 0x1f) << 8) | pkt[2] as u16;
        if pid != self.pid {
            return false;
        }
        let adapt_ctrl = (pkt[3] >> 4) & 0x3;
        let mut off = 4usize;
        if adapt_ctrl == 2 || adapt_ctrl == 3 {
            if off >= pkt.len() {
                return false;
            }
            off += 1 + pkt[off] as usize;
        }
        if adapt_ctrl == 2 || off >= pkt.len() {
            return false; // no payload
        }
        let pusi = pkt[1] & 0x40 != 0;
        let cc = pkt[3] & 0x0f;
        let payload = &pkt[off..];

        // CC gap detection: flag current partial as corrupted if a packet was dropped.
        // PUSI always resets the section, so CC is only checked for non-PUSI packets.
        if !pusi {
            if let Some(prev) = self.last_cc {
                let expected = (prev + 1) & 0x0f;
                if cc != expected {
                    // Packet(s) were dropped — current partial is corrupted.
                    self.partial.clear();
                    self.partial_valid = false;
                    self.cc_drops += 1;
                }
            }
        }
        self.last_cc = Some(cc);

        if pusi {
            let ptr = payload[0] as usize;
            let finish_bytes = ptr.min(payload.len().saturating_sub(1));
            // Finish pending section only if no CC gaps were seen for it.
            if self.partial_valid {
                self.partial
                    .extend_from_slice(&payload[1..1 + finish_bytes]);
                self.try_complete();
            }
            // Start new section — valid from here until a CC gap is seen.
            self.partial.clear();
            self.partial_valid = true;
            let new_off = 1 + ptr;
            if new_off < payload.len() {
                self.partial.extend_from_slice(&payload[new_off..]);
                self.try_complete();
            }
        } else {
            if self.partial_valid {
                self.partial.extend_from_slice(payload);
                self.try_complete();
            }
        }

        !self.ready.is_empty()
    }

    fn try_complete(&mut self) {
        loop {
            if self.partial.len() < 3 {
                break;
            }
            // Skip stuffing bytes (0xFF)
            if self.partial[0] == 0xff {
                let skip = self.partial.iter().take_while(|&&b| b == 0xff).count();
                self.partial.drain(..skip);
                continue;
            }
            let slen = ((self.partial[1] as usize & 0x0f) << 8) | self.partial[2] as usize;
            let total = 3 + slen;
            if total > 4096 {
                self.partial.clear();
                break;
            }
            if self.partial.len() < total {
                break;
            }
            self.ready.push(self.partial[..total].to_vec());
            self.partial.drain(..total);
        }
    }

    /// Drain completed sections.
    pub fn drain(&mut self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.ready.drain(..)
    }
}

// ── Content descriptor genre names (ARIB STD-B10 table 6-2) ─────────────────

pub fn content_genre(l1: u8, _l2: u8) -> &'static str {
    match l1 {
        0x0 => "ニュース/報道",
        0x1 => "スポーツ",
        0x2 => "情報/ワイドショー",
        0x3 => "ドラマ",
        0x4 => "音楽",
        0x5 => "バラエティ",
        0x6 => "映画",
        0x7 => "アニメ/特撮",
        0x8 => "ドキュメンタリー/教養",
        0x9 => "劇場/公演",
        0xa => "趣味/教育",
        0xb => "福祉",
        0xf => "その他",
        _ => "不明",
    }
}
