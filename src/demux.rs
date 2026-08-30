use crate::dtv_cmds::MYTHOLOGICAL_FULLMUX_PID;
use crate::error::{Error, Result};
use crate::sys::dmx_abi::{
    dmx_apply_pes_filter, dmx_stream_start, dmx_stream_stop, DmxPesFilterParams,
};
use libc::{O_CLOEXEC, O_NONBLOCK};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub fn demux_path(adapter: u32, index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/dvb/adapter{adapter}/demux{index}"))
}

pub fn dvr_path(adapter: u32, index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/dvb/adapter{adapter}/dvr{index}"))
}

pub struct Demux {
    fd: std::fs::File,
}

impl Demux {
    pub fn open_rw(adapter: u32, index: u32) -> Result<Self> {
        let p = demux_path(adapter, index);
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_CLOEXEC)
            .open(&p)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Error::Msg(format!("open {}: {}", p.display(), e))
                } else {
                    e.into()
                }
            })?;
        Ok(Self { fd })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }

    /// Section filter on `pid` (e.g. `0` for PAT); read filtered sections with [`Demux::read`].
    pub fn set_section_filter(
        &self,
        params: &crate::sys::dmx_abi::DmxSctFilterParams,
    ) -> Result<()> {
        unsafe {
            crate::sys::dmx_abi::dmx_apply_section_filter(self.as_raw_fd(), params)?;
            crate::sys::dmx_abi::dmx_stream_start(self.as_raw_fd())?;
        }
        Ok(())
    }
    pub fn zap_all_pids_to_dvr(&self) -> Result<()> {
        let p = DmxPesFilterParams::ts_tap_all_pids_mythological(MYTHOLOGICAL_FULLMUX_PID);
        unsafe {
            dmx_apply_pes_filter(self.as_raw_fd(), &p)?;
            dmx_stream_start(self.as_raw_fd())?;
        }
        Ok(())
    }

    /// Route one TS PID to the shared DVR device with DMX_OUT_TS_TAP.
    pub fn tap_pid_to_dvr(&self, pid: u16) -> Result<()> {
        let p = DmxPesFilterParams::ts_tap_pid(pid);
        unsafe {
            dmx_apply_pes_filter(self.as_raw_fd(), &p)?;
            dmx_stream_start(self.as_raw_fd())?;
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        unsafe {
            let _ = dmx_stream_stop(self.as_raw_fd());
        }
        Ok(())
    }
}

impl Drop for Demux {
    fn drop(&mut self) {
        unsafe {
            let _ = dmx_stream_stop(self.as_raw_fd());
        }
    }
}

pub struct PidTaps {
    taps: Vec<Demux>,
}

impl PidTaps {
    pub fn start(adapter: u32, index: u32, pids: &[u16]) -> Result<Self> {
        let mut pids = pids.to_vec();
        pids.sort_unstable();
        pids.dedup();

        let mut taps = Vec::with_capacity(pids.len());
        for pid in pids {
            let dm = Demux::open_rw(adapter, index)?;
            dm.tap_pid_to_dvr(pid)?;
            taps.push(dm);
        }

        Ok(Self { taps })
    }

    pub fn len(&self) -> usize {
        self.taps.len()
    }
}

pub struct DvrReader {
    fd: std::fs::File,
}

impl DvrReader {
    pub fn open_ro(adapter: u32, index: u32) -> Result<Self> {
        Self::open_with_flags(adapter, index, O_CLOEXEC)
    }

    pub fn open_ro_nonblocking(adapter: u32, index: u32) -> Result<Self> {
        Self::open_with_flags(adapter, index, O_CLOEXEC | O_NONBLOCK)
    }

    fn open_with_flags(adapter: u32, index: u32, flags: i32) -> Result<Self> {
        let p = dvr_path(adapter, index);
        let fd = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(&p)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Error::Msg(format!("open {}: {}", p.display(), e))
                } else {
                    e.into()
                }
            })?;
        Ok(Self { fd })
    }

    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.as_raw_fd()
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }

    pub fn copy_to(&mut self, out: &mut impl Write, buf: &mut [u8]) -> Result<()> {
        loop {
            let n = self.fd.read(buf)?;
            if n == 0 {
                return Ok(());
            }
            out.write_all(&buf[..n])?;
        }
    }
}

impl Read for DvrReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
}
