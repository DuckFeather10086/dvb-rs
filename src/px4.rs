//! px4_drv chardev backend: `/dev/px4video{N}`.
//!
//! PLEX PX4/PX5/PX-MLT series, the newer PX-S1UR/PX-M1UR, and the
//! e-Better DTV series expose their tuners as character devices
//! (`/dev/px4video0`, `/dev/px4video1`, … one per tuner) instead of DVB
//! adapters. There is no kernel-side tuning, PID filtering or section
//! demux: the px4_drv module does the chip work (IT930x bridge, tuner,
//! demod) and hands the *full transport stream* to userspace. This
//! module is the userspace half of that contract:
//!
//!   1. `PTX_SET_CHANNEL` picks a physical channel (ISDB-T UHF ch 13..62
//!      is `freq_no` 63..112, i.e. ch + 50);
//!   2. `PTX_START_STREAMING`, then `read()` yields the whole mux;
//!   3. PAT/PMT parsing, PID taps and section assembly happen here in
//!      userspace, because there is no demux device to ask.
//!
//! The ioctl numbers and struct layouts are the driver's public userspace
//! ABI (`include/ptx_ioctl.h`); the same definitions power recisdb-rs.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use crate::eit::TS_PACKET_SIZE;
use crate::error::{Error, Result};

/// The device node for adapter N. px4_drv numbers tuners across the
/// hardware: a PX-Q3U4's four tuners are `/dev/px4video0..3`, matching
/// the adapter numbers ferrite's pool hands to `--adapter`.
pub fn device_path(adapter: u32) -> String {
    format!("/dev/px4video{adapter}")
}

/// `struct ptx_freq` (ptx_ioctl.h): `freq_no` is a system-dependent
/// channel number, `slot` a stream id (0 for terrestrial).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtxFreq {
    pub freq_no: i32,
    pub slot: i32,
}

// PTX_GET_CNR is declared `_IOR(0x8d, 0x04, int *)` in the header, which
// sizes the ioctl at 8 on LP64 while the driver writes only 4 bytes of
// CNR. Reading a u64 keeps the number right and the upper half zero.
nix::ioctl_write_ptr!(set_channel, 0x8d, 0x01, PtxFreq);
nix::ioctl_none!(start_streaming, 0x8d, 0x02);
nix::ioctl_none!(stop_streaming, 0x8d, 0x03);
nix::ioctl_read!(get_cnr, 0x8d, 0x04, u64);

/// ISDB-T UHF plan: physical ch 13 centred at 473.142857 MHz (1/7 MHz
/// carrier spacing), 6 MHz apart — the same constants ferrite's scan
/// module uses.
const UHF_BASE: u32 = 473_142_857;
const UHF_SPACING: u32 = 6_000_000;
const FIRST_PHYSICAL: u32 = 13;
const LAST_PHYSICAL: u32 = 62;

/// px4_drv numbers terrestrial UHF channels 13..62 as `freq_no` 63..112.
pub fn freq_no_for_physical(ch: u16) -> i32 {
    ch as i32 + 50
}

/// Reverse of `UHF_BASE + (ch − 13)·6 MHz`: the physical channel a
/// channels.json FREQUENCY tunes on the px4 backend.
///
/// Only UHF 13..62 is accepted. CATV passthrough frequencies (C13..C63,
/// below 473 MHz) use a different `freq_no` range and the extended
/// `PTXT_*` interface; rejecting them here is what keeps a cable channel
/// from being tuned as a wrong-frequency terrestrial one.
pub fn physical_from_frequency(freq: u32) -> Result<u16> {
    if freq < UHF_BASE {
        return Err(Error::Msg(format!(
            "frequency {freq} Hz is below UHF 13 — CATV passthrough is not \
             supported on the px4 backend"
        )));
    }
    let offset = freq - UHF_BASE;
    let ch = offset / UHF_SPACING + FIRST_PHYSICAL;
    if ch > LAST_PHYSICAL {
        return Err(Error::Msg(format!(
            "frequency {freq} Hz is above UHF 62 (would be channel {ch})"
        )));
    }
    if offset % UHF_SPACING > 100_000 {
        return Err(Error::Msg(format!(
            "frequency {freq} Hz is not on a UHF 13-62 centre"
        )));
    }
    Ok(ch as u16)
}

/// One open `/dev/px4video{N}` device. Nonblocking, so a tune with no
/// signal reports a stall instead of blocking forever.
pub struct Px4Device {
    file: File,
    streaming: bool,
}

impl Px4Device {
    pub fn open(adapter: u32) -> Result<Self> {
        let path = device_path(adapter);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Error::Msg(format!(
                        "open {path}: no such device (is the px4_drv module loaded?)"
                    ))
                } else {
                    e.into()
                }
            })?;
        Ok(Self {
            file,
            streaming: false,
        })
    }

    /// Tune to an ISDB-T physical channel and start streaming. The
    /// frontend locks asynchronously; TS starts arriving within a few
    /// hundred ms and the callers' read loops time out on their own.
    pub fn tune_isdbt(&mut self, physical_ch: u16) -> Result<()> {
        let freq = PtxFreq {
            freq_no: freq_no_for_physical(physical_ch),
            slot: 0,
        };
        unsafe { set_channel(self.file.as_raw_fd(), &freq) }?;
        unsafe { start_streaming(self.file.as_raw_fd()) }?;
        self.streaming = true;
        Ok(())
    }

    pub fn stop_streaming(&mut self) {
        if self.streaming {
            unsafe {
                let _ = stop_streaming(self.file.as_raw_fd());
            }
            self.streaming = false;
        }
    }

    /// Raw CNR as reported by the driver (0..65535; the exact meaning
    /// varies by frontend chip).
    pub fn cnr(&self) -> Result<u32> {
        let mut raw = 0u64;
        unsafe { get_cnr(self.file.as_raw_fd(), &mut raw) }?;
        Ok(raw as u32)
    }
}

impl Read for Px4Device {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Drop for Px4Device {
    fn drop(&mut self) {
        self.stop_streaming();
    }
}

/// Userspace TS PID filter: reads 188-byte packets from `inner` and
/// forwards only those whose PID is in `keep`, preserving packet
/// alignment. This is the px4 analogue of the DVB kernel PID tap —
/// there is no demux to filter in the driver.
///
/// Complete kept packets are staged whole and served out byte-continuous,
/// so the caller may read into any buffer size without losing alignment;
/// the only consumers that parse packets (the section assemblers) pass
/// 188-byte multiples anyway.
pub struct TsPidFilter<R: Read> {
    inner: R,
    keep: Vec<u16>, // sorted, for binary_search
    carry: Vec<u8>, // raw bytes from inner, possibly mid-packet
    staged: Vec<u8>, // complete kept packets, ready to serve
}

impl<R: Read> TsPidFilter<R> {
    pub fn new(inner: R, keep: &[u16]) -> Self {
        let mut keep = keep.to_vec();
        keep.sort_unstable();
        keep.dedup();
        Self {
            inner,
            keep,
            carry: Vec::with_capacity(TS_PACKET_SIZE * 8),
            staged: Vec::with_capacity(TS_PACKET_SIZE * 8),
        }
    }

    /// Move complete, kept packets from `carry` into `staged`.
    fn ingest(&mut self) {
        // Resync: a leading byte that is not 0x47 means the stream was
        // misaligned; drop bytes until the next sync candidate.
        while !self.carry.is_empty() && self.carry[0] != 0x47 {
            self.carry.remove(0);
        }
        let mut consumed = 0;
        while self.carry.len() - consumed >= TS_PACKET_SIZE {
            let pkt = &self.carry[consumed..consumed + TS_PACKET_SIZE];
            let pid = ((pkt[1] as u16 & 0x1f) << 8) | pkt[2] as u16;
            if self.keep.binary_search(&pid).is_ok() {
                self.staged.extend_from_slice(pkt);
            }
            consumed += TS_PACKET_SIZE;
        }
        if consumed > 0 {
            self.carry.drain(..consumed);
        }
    }

    /// Copy what we can of `staged` into `buf`, draining it.
    fn serve(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.staged.len());
        buf[..n].copy_from_slice(&self.staged[..n]);
        self.staged.drain(..n);
        n
    }
}

impl<R: Read> Read for TsPidFilter<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.staged.is_empty() {
            let mut tmp = [0u8; TS_PACKET_SIZE * 64];
            // Pull from inner until a kept packet is staged, the source
            // ends, or it would block. The bound keeps a pathological
            // stream (sync bytes but no kept PID, ever) from spinning.
            for _ in 0..4096 {
                match self.inner.read(&mut tmp) {
                    Ok(0) => return Ok(0),
                    Ok(n) => {
                        self.carry.extend_from_slice(&tmp[..n]);
                        self.ingest();
                        if !self.staged.is_empty() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Nothing available right now; carry keeps any
                        // partial packet. Caller decides what silence
                        // means (the tune loop's stall watchdog).
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(self.serve(buf))
    }
}

/// Read every section with `table_id` arriving on `pid` from the device's
/// live stream, reassembling sections in userspace.
///
/// The px4 analogue of `si_reader::read_sections`: where the DVB path
/// gets its PID reduction from a kernel demux tap, this one filters the
/// full mux in userspace and feeds the shared assembly loop. Fails with
/// [`Error::Si`] when nothing arrived before `timeout` — which is also
/// what a tune with no signal looks like, since the PAT never comes.
pub fn read_sections_px4(
    dev: &mut Px4Device,
    pid: u16,
    table_id: u8,
    timeout: Duration,
    first_only: bool,
) -> Result<Vec<Vec<u8>>> {
    let mut filtered = TsPidFilter::new(dev, &[pid]);
    let out = crate::si_reader::read_sections_from_source(
        &mut filtered,
        pid,
        table_id,
        timeout,
        first_only,
    )?;
    if out.is_empty() {
        return Err(Error::Si(format!(
            "timed out reading table 0x{table_id:02x} from PID 0x{pid:04x}"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn freq_no(ch: u16) -> i32 {
        freq_no_for_physical(ch)
    }

    #[test]
    fn uhf_frequencies_map_to_physical_channels() {
        // 13ch is the ISDB-T plan's base; 62ch the top of the band.
        assert_eq!(physical_from_frequency(473_142_857).unwrap(), 13);
        assert_eq!(physical_from_frequency(539_142_857).unwrap(), 24);
        assert_eq!(physical_from_frequency(767_142_857).unwrap(), 62);
        // Not on a 6 MHz centre.
        assert!(physical_from_frequency(540_000_000).is_err());
        // CATV passthrough and out-of-band.
        assert!(physical_from_frequency(225_142_857).is_err());
        assert!(physical_from_frequency(800_000_000).is_err());
        // The ioctl number is ch + 50 (63..112 for UHF 13..62).
        assert_eq!(freq_no(13), 63);
        assert_eq!(freq_no(62), 112);
        assert_eq!(freq_no(24), 74);
    }

    /// Build one TS packet. `payload` is split across packets by the
    /// caller; `pusi` marks the start of a section; `pid` selects.
    fn pkt(pid: u16, cc: u8, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; TS_PACKET_SIZE];
        p[0] = 0x47;
        p[1] = ((pid >> 8) as u8 & 0x1f) | if pusi { 0x40 } else { 0 };
        p[2] = pid as u8;
        p[3] = (0x10) | (cc & 0x0f); // payload only
        if pusi {
            // pointer_field: section starts at payload[1]
            p[4] = 0;
            p[5..5 + payload.len()].copy_from_slice(payload);
        } else {
            p[4..4 + payload.len()].copy_from_slice(payload);
        }
        p
    }

    /// One complete single-section SI table on `pid`.
    fn section_packet(pid: u16, cc: u8, table_id: u8, section_num: u8, last: u8) -> Vec<u8> {
        // section_length = 13 payload; section: table_id, flags+len,
        // tsid(2) ver(1) sec(1) last(1), crc(4).
        let payload: Vec<u8> = vec![
            table_id,
            0xf0,
            13, // section_length low byte (high nibble 0)
            0x00,
            0x01, // tsid
            0xc1, // version + current
            section_num,
            last,
            0xde,
            0xad,
            0xbe,
            0xef,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        pkt(pid, cc, true, &payload)
    }

    #[test]
    fn pid_filter_keeps_only_wanted_pids() {
        let wanted = vec![0x0000, 0x0100];
        let mut stream = Vec::new();
        for pid in [0x0000u16, 0x0100, 0x0101, 0x1fff] {
            stream.extend(pkt(pid, 0, true, &[0xaa; 100]));
        }
        let src = std::io::Cursor::new(stream);
        let mut f = TsPidFilter::new(src, &wanted);
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE);
        for chunk in out.chunks_exact(TS_PACKET_SIZE) {
            let pid = ((chunk[1] as u16 & 0x1f) << 8) | chunk[2] as u16;
            assert!(wanted.contains(&pid), "unwanted pid {pid} leaked");
        }
    }

    #[test]
    fn pid_filter_resyncs_and_handles_partial_packets() {
        // The stream starts mid-packet (no 0x47 at byte 0): the filter
        // must drop the run-in until a sync candidate, then emit whole
        // packets. A later partial read (packet split across reads) must
        // not lose alignment either.
        let p = pkt(0x0100, 0, true, &[0xbb; 100]);
        let mut stream = Vec::new();
        stream.extend_from_slice(&p[100..]); // run-in: payload bytes, no sync
        stream.extend_from_slice(&p);
        stream.extend(pkt(0x0100, 1, false, &[0xcc; 100]));

        // Give the filter its input in awkward slices so packets span
        // multiple reads.
        let src = std::io::Cursor::new(stream);
        let mut f = TsPidFilter::new(src, &[0x0100]);
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE);
        assert!(out.chunks_exact(TS_PACKET_SIZE).all(|w| w[0] == 0x47));
    }

    #[test]
    fn empty_stream_stalls() {
        // A source that never yields data (no signal) must surface as a
        // stall (read returns 0), not an error or a spin.
        let src = std::io::Cursor::new(Vec::new());
        let mut f = TsPidFilter::new(src, &[0x0000]);
        let mut out = [0u8; 188];
        assert_eq!(f.read(&mut out).unwrap(), 0);
    }
}
