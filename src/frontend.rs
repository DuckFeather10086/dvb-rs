use crate::error::{Error, Result};
use crate::sys::ffi::{dtv_properties, dtv_property, dvb_frontend_info};
use crate::sys::ioctl;
use libc::O_CLOEXEC;
use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub fn frontend_path(adapter: u32, index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/dvb/adapter{adapter}/frontend{index}"))
}

pub struct Frontend {
    fd: std::fs::File,
}

impl Frontend {
    pub fn open_rw(adapter: u32, index: u32) -> Result<Self> {
        let p = frontend_path(adapter, index);
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_CLOEXEC)
            .open(&p)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
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

    pub fn set_properties(&self, props: &mut [dtv_property]) -> Result<()> {
        let cmd = dtv_properties {
            num: props
                .len()
                .try_into()
                .map_err(|_| Error::Msg("too many dtv properties".into()))?,
            props: props.as_mut_ptr(),
        };
        unsafe {
            ioctl::fe_apply_properties(self.as_raw_fd(), &cmd)?;
        }
        Ok(())
    }

    pub fn get_frontend_info(&self, info: &mut dvb_frontend_info) -> Result<()> {
        unsafe {
            ioctl::fe_get_info(self.as_raw_fd(), info)?;
        }
        Ok(())
    }
}
