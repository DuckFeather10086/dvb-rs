//! Reading SI tables (PAT / PMT / SDT / EIT) off a live transport.
//!
//! **Why this exists instead of `DMX_SET_FILTER`.** The kernel demux can
//! filter and deliver whole sections by itself, which is the obvious way to
//! read a table — but the Siano/smsusb driver does not implement section
//! filtering. A `set_section_filter` + read on that hardware blocks forever
//! (no error, no data), which is exactly how `dvbr scan` came to hang on
//! every invocation while `dvbr tune` — which already used the approach in
//! this module — worked fine.
//!
//! So: tap the PID straight through to the DVR device and reassemble
//! sections from the raw TS packets in userspace ([`TsSectionAssembler`]).
//! Slower and a little more code, but it works on every driver.
//!
//! The px4 chardev backend has the same problem one layer up: there is no
//! kernel demux at all, so its PID reduction happens in userspace
//! ([`crate::px4::TsPidFilter`]) and the sections are assembled by the
//! same [`read_sections_from_source`] loop.
//!
//! Anything in this crate that needs an SI table should go through here.
//! Do not reintroduce `DMX_SET_FILTER` without testing on smsusb hardware.

use std::io;
use std::time::{Duration, Instant};

use crate::demux::{Demux, DvrReader};
use crate::eit::{TsSectionAssembler, TS_PACKET_SIZE};
use crate::error::{Error, Result};

/// Default budget for reading one table. PAT/SDT repeat every few hundred
/// milliseconds on a locked transport, so this is generous.
pub const SI_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Section-header fields shared by all long-form (syntax indicator set)
/// SI tables: PAT, PMT, SDT, EIT.
fn section_numbers(section: &[u8]) -> Option<(u8, u8)> {
    if section.len() < 8 || section[1] & 0x80 == 0 {
        return None;
    }
    Some((section[6], section[7]))
}

/// Read the first section with `table_id` from `pid`.
pub fn read_section(
    adapter: u32,
    demux: u32,
    pid: u16,
    table_id: u8,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut sections = read_sections(adapter, demux, pid, table_id, timeout, true)?;
    Ok(sections.remove(0))
}

/// Read every section of `table_id` on `pid`.
///
/// Multi-section tables (an SDT for a mux with many services, for instance)
/// are collected until each index up to `last_section_number` has been seen,
/// or the timeout expires — whatever sections did arrive are returned rather
/// than failing, since a partial SDT still names some services.
///
/// With `first_only`, returns as soon as one section matches.
pub fn read_sections(
    adapter: u32,
    demux: u32,
    pid: u16,
    table_id: u8,
    timeout: Duration,
    first_only: bool,
) -> Result<Vec<Vec<u8>>> {
    let dm = Demux::open_rw(adapter, demux)?;
    dm.tap_pid_to_dvr(pid)?;
    let mut dvr = DvrReader::open_ro_nonblocking(adapter, demux)?;
    // The kernel demux tap has already reduced the stream to `pid`, so the
    // generic loop gets exactly the packets it wants.
    let out = read_sections_from_source(&mut dvr, pid, table_id, timeout, first_only)?;
    drop(dm);
    if out.is_empty() {
        return Err(Error::Si(format!(
            "timed out reading table 0x{table_id:02x} from PID 0x{pid:04x}"
        )));
    }
    Ok(out)
}

/// Read every section with `table_id` arriving on `pid` from a live
/// source, reassembling sections in userspace.
///
/// The DVB path reaches this with the stream already reduced to `pid` by
/// a kernel demux tap; the px4 path reaches it through a
/// [`crate::px4::TsPidFilter`] over the chardev's full mux. Either way
/// the assembler only ever sees its own PID.
///
/// With `first_only`, returns as soon as one section matches. Returns
/// whatever arrived when `timeout` expires, possibly empty.
pub(crate) fn read_sections_from_source<R: io::Read>(
    src: &mut R,
    pid: u16,
    table_id: u8,
    timeout: Duration,
    first_only: bool,
) -> Result<Vec<Vec<u8>>> {
    let mut asm = TsSectionAssembler::new(pid);
    let mut buf = vec![0u8; TS_PACKET_SIZE * 64];
    let deadline = Instant::now() + timeout;

    // Indexed by section_number so a repeating table doesn't accumulate
    // duplicates; a table cycles continuously on the wire.
    let mut collected: Vec<Option<Vec<u8>>> = Vec::new();
    let mut last_section: Option<u8> = None;

    while Instant::now() < deadline {
        let n = match src.read(&mut buf) {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };

        for pkt in buf[..n].chunks_exact(TS_PACKET_SIZE) {
            if !asm.feed(pkt) {
                continue;
            }
            for section in asm.drain() {
                if section.first().copied() != Some(table_id) {
                    continue;
                }
                if first_only {
                    return Ok(vec![section]);
                }
                let (num, last) = match section_numbers(&section) {
                    Some(v) => v,
                    // Short-form section: no numbering to track, take it.
                    None => return Ok(vec![section]),
                };
                last_section = Some(last);
                let idx = num as usize;
                if collected.len() <= idx {
                    collected.resize(idx + 1, None);
                }
                collected[idx] = Some(section);
            }
        }

        // Complete once every index up to last_section_number is present.
        if let Some(last) = last_section {
            let want = last as usize + 1;
            if collected.len() >= want && collected[..want].iter().all(Option::is_some) {
                break;
            }
        }
    }

    Ok(collected.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pkt(pid: u16, cc: u8, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; TS_PACKET_SIZE];
        p[0] = 0x47;
        p[1] = ((pid >> 8) as u8 & 0x1f) | if pusi { 0x40 } else { 0 };
        p[2] = pid as u8;
        p[3] = 0x10 | (cc & 0x0f);
        if pusi {
            p[4] = 0;
            p[5..5 + payload.len()].copy_from_slice(payload);
        } else {
            p[4..4 + payload.len()].copy_from_slice(payload);
        }
        p
    }

    fn section_packet(pid: u16, cc: u8, table_id: u8, section_num: u8, last: u8) -> Vec<u8> {
        let payload: Vec<u8> = vec![
            table_id, 0xf0, 13, 0x00, 0x01, 0xc1, section_num, last, 0xde, 0xad, 0xbe, 0xef,
            0x00, 0x00, 0x00, 0x00,
        ];
        pkt(pid, cc, true, &payload)
    }

    #[test]
    fn section_numbers_reads_long_form_header() {
        // table_id, syntax+len, ..., section_number=2, last=5
        let s = [0x42u8, 0xf0, 0x10, 0, 0, 0, 2, 5];
        assert_eq!(section_numbers(&s), Some((2, 5)));
    }

    #[test]
    fn section_numbers_rejects_short_form_and_runts() {
        // Syntax indicator clear → no section numbering.
        let short_form = [0x42u8, 0x70, 0x10, 0, 0, 0, 0, 0];
        assert_eq!(section_numbers(&short_form), None);
        assert_eq!(section_numbers(&[0x42u8, 0xf0]), None);
    }

    #[test]
    fn generic_loop_reads_first_section_of_a_pid() {
        let mut stream = Vec::new();
        stream.extend(section_packet(0x0000, 0, 0x00, 0, 0));
        stream.extend(section_packet(0x0100, 0, 0x42, 0, 0));
        let mut src = Cursor::new(stream);
        let sections =
            read_sections_from_source(&mut src, 0x0000, 0x00, Duration::from_secs(1), true)
                .unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0][0], 0x00);
    }

    #[test]
    fn generic_loop_collects_multi_section_tables() {
        let mut stream = Vec::new();
        for num in [0u8, 1u8] {
            stream.extend(section_packet(0x0011, num, 0x42, num, 1));
        }
        let mut src = Cursor::new(stream);
        let sections =
            read_sections_from_source(&mut src, 0x0011, 0x42, Duration::from_secs(1), false)
                .unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0][6], 0);
        assert_eq!(sections[1][6], 1);
    }

    #[test]
    fn generic_loop_times_out_empty() {
        let mut src = Cursor::new(Vec::new());
        let sections =
            read_sections_from_source(&mut src, 0x0000, 0x00, Duration::from_millis(30), false)
                .unwrap();
        assert!(sections.is_empty());
    }
}
