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

pub const DMX_IMMEDIATE_START_PES: u32 = 4;

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

    /// Capture a single PID as TS to the DVR device (DMX_OUT_TS_TAP).
    pub fn ts_tap_pid(pid: u16) -> Self {
        Self {
            pid,
            _pad: 0,
            input: DmxInput::Frontend as u32,
            output: DmxOutput::TsTap as u32,
            pes_type: DmxPesType::Other as u32,
            flags: DMX_IMMEDIATE_START_PES,
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

    /// EIT present/following (table_id 0x4E), no service_id filter.
    /// `kernel_timeout_ms`: kernel auto-stops filter after this many ms of no match (0 = disabled).
    pub fn eit_pf(pid: u16, kernel_timeout_ms: u32) -> Self {
        let mut fl = DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        };
        fl.filter[0] = 0x4e;
        fl.mask[0] = 0xff;
        // No DMX_CHECK_CRC: some drivers drop EIT when CRC mismatch under marginal signal.
        Self {
            pid,
            _pad0: 0,
            filter: fl,
            timeout: kernel_timeout_ms,
            flags: DMX_IMMEDIATE_START,
        }
    }

    /// EIT present/following (table_id 0x4E) filtered by service_id.
    pub fn eit_pf_for_service(pid: u16, service_id: u16) -> Self {
        let mut fl = DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        };
        fl.filter[0] = 0x4e;
        fl.mask[0] = 0xff;
        fl.filter[3] = (service_id >> 8) as u8;
        fl.mask[3] = 0xff;
        fl.filter[4] = service_id as u8;
        fl.mask[4] = 0xff;
        Self {
            pid,
            _pad0: 0,
            filter: fl,
            timeout: 0,
            flags: DMX_CHECK_CRC | DMX_IMMEDIATE_START,
        }
    }

    /// Section filter that passes ALL sections on `pid` (no table_id or service_id filtering).
    /// Useful for EIT on PID 0x0012 where we want all table_ids (0x4E p/f + 0x50–0x5F schedule).
    pub fn any_on_pid(pid: u16) -> Self {
        let fl = DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        };
        Self {
            pid,
            _pad0: 0,
            filter: fl,
            timeout: 0,
            flags: DMX_IMMEDIATE_START,
        }
    }

    /// EIT schedule (table_id 0x50–0x5F) filtered by service_id.
    /// Uses mode[0]=0xFF to match table_id range 0x50–0x5F via mask 0xF0.
    pub fn eit_schedule_for_service(pid: u16, service_id: u16) -> Self {
        let mut fl = DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        };
        fl.filter[0] = 0x50;
        fl.mask[0] = 0xf0; // match 0x50–0x5F
        fl.filter[3] = (service_id >> 8) as u8;
        fl.mask[3] = 0xff;
        fl.filter[4] = service_id as u8;
        fl.mask[4] = 0xff;
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
