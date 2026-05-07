use crate::error::Result;
use crate::frontend::Frontend;
use crate::sys::ioctl;

#[derive(Debug, Clone, Default)]
pub struct SignalStats {
    pub lock_mask: u32,
    pub signal_strength_0_ffff: Option<u16>,
    pub snr_0_ffff: Option<u16>,
}

pub fn read_stats(fe: &Frontend) -> Result<SignalStats> {
    let mut mask = 0u32;
    unsafe {
        ioctl::fe_read_lock_status(fe.as_raw_fd(), &mut mask)?;
    }

    let mut ss = SignalStats {
        lock_mask: mask,
        signal_strength_0_ffff: None,
        snr_0_ffff: None,
    };

    let mut v = 0u16;
    if unsafe { ioctl::fe_read_signal_strength_legacy(fe.as_raw_fd(), &mut v) }.is_ok() {
        ss.signal_strength_0_ffff = Some(v);
    }
    v = 0;
    if unsafe { ioctl::fe_read_snr_legacy(fe.as_raw_fd(), &mut v) }.is_ok() {
        ss.snr_0_ffff = Some(v);
    }

    Ok(ss)
}
