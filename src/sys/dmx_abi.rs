//! Linux `DMX_*` types and ioctl wrappers (`linux/dvb/dmx.h`).

use std::os::fd::RawFd;

/// `DMX_IN_FRONTEND` / `DMX_IN_DVR`
#[repr(u32)]
#[derive(Debug, Copy, Clone)]
pub enum DmxInput {
    Frontend = 0,
    Dvr = 1,
}

/// Demux output target (`dmx_output`).
#[repr(u32)]
#[derive(Debug, Copy, Clone)]
pub enum DmxOutput {
    Decoder = 0,
    Tap = 1,
    TsTap = 2,
    TsDemuxTap = 3,
}

/// `DMX_PES_*` (last variant = `DMX_PES_OTHER` = 20).
#[repr(u32)]
#[derive(Debug, Copy, Clone)]
pub enum DmxPesType {
    Other = 20,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct DmxPesFilterParams {
    pub pid: u16,
    _pad: u16,
    pub input: u32,
    pub output: u32,
    pub pes_type: u32,
    pub flags: u32,
}

impl DmxPesFilterParams {
    pub fn ts_tap_all_pids_mythological(pid: u16) -> Self {
        Self {
            pid,
            _pad: 0,
            input: DmxInput::Frontend as u32,
            output: DmxOutput::TsTap as u32,
            pes_type: DmxPesType::Other as u32,
            flags: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct DmxFilter {
    pub filter: [u8; 16],
    pub mask: [u8; 16],
    pub mode: [u8; 16],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct DmxSctFilterParams {
    pub pid: u16,
    _pad0: u16,
    pub filter: DmxFilter,
    pub timeout: u32,
    pub flags: u32,
}

pub const DMX_CHECK_CRC: u32 = 1;
pub const DMX_IMMEDIATE_START: u32 = 4;

impl DmxSctFilterParams {
    pub fn pat(pid: u16) -> Self {
        let mut fl = DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        };
        fl.filter[0] = 0x00;
        fl.mask[0] = 0xff;
        Self {
            pid,
            _pad0: 0,
            filter: fl,
            timeout: 0,
            flags: DMX_CHECK_CRC | DMX_IMMEDIATE_START,
        }
    }

    pub fn sdt_actual(pid: u16) -> Self {
        let mut fl = DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        };
        fl.filter[0] = 0x42;
        fl.mask[0] = 0xff;
        Self {
            pid,
            _pad0: 0,
            filter: fl,
            timeout: 0,
            flags: DMX_CHECK_CRC | DMX_IMMEDIATE_START,
        }
    }
}

nix::ioctl_write_ptr!(dmx_set_pes_filter, b'o', 44, DmxPesFilterParams);
nix::ioctl_write_ptr!(dmx_set_filter, b'o', 43, DmxSctFilterParams);
nix::ioctl_none!(dmx_start, b'o', 41);
nix::ioctl_none!(dmx_stop, b'o', 42);

pub unsafe fn dmx_apply_pes_filter(fd: RawFd, p: &DmxPesFilterParams) -> nix::Result<()> {
    unsafe { dmx_set_pes_filter(fd, p as *const _).map(|_| ()) }
}

pub unsafe fn dmx_apply_section_filter(fd: RawFd, p: &DmxSctFilterParams) -> nix::Result<()> {
    unsafe { dmx_set_filter(fd, p as *const _).map(|_| ()) }
}

pub unsafe fn dmx_stream_start(fd: RawFd) -> nix::Result<()> {
    unsafe { dmx_start(fd).map(|_| ()) }
}

pub unsafe fn dmx_stream_stop(fd: RawFd) -> nix::Result<()> {
    unsafe { dmx_stop(fd).map(|_| ()) }
}
