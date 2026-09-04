//! `FE_*` ioctl wrappers (nix).

use crate::sys::ffi::{dtv_properties, dvb_frontend_info};
use std::os::fd::RawFd;

nix::ioctl_write_ptr!(fe_set_property, b'o', 82, dtv_properties);
nix::ioctl_read!(fe_get_property, b'o', 83, dtv_properties);
nix::ioctl_read!(fe_get_frontend_info, b'o', 61, dvb_frontend_info);
nix::ioctl_read!(fe_read_status, b'o', 69, u32);
nix::ioctl_read!(fe_read_signal_strength_legacy, b'o', 71, u16);
nix::ioctl_read!(fe_read_snr_legacy, b'o', 72, u16);

pub unsafe fn fe_read_lock_status(fd: RawFd, mask: &mut u32) -> nix::Result<()> {
    unsafe { fe_read_status(fd, mask as *mut _).map(|_| ()) }
}

pub unsafe fn fe_get_info(fd: RawFd, info: &mut dvb_frontend_info) -> nix::Result<()> {
    unsafe { fe_get_frontend_info(fd, info as *mut _).map(|_| ()) }
}

pub unsafe fn fe_apply_properties(fd: RawFd, props: &dtv_properties) -> nix::Result<()> {
    unsafe { fe_set_property(fd, props as *const _).map(|_| ()) }
}

/// `FE_GET_PROPERTY`: the driver fills in the `u` union of each property
/// whose `cmd` was set by the caller. Takes `&mut` because the ioctl is
/// declared `_IOWR` and writes through the pointer.
pub unsafe fn fe_read_properties(fd: RawFd, props: &mut dtv_properties) -> nix::Result<()> {
    unsafe { fe_get_property(fd, props as *mut _).map(|_| ()) }
}
