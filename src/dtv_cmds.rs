//! `DTV_*` command constants from `linux/dvb/frontend.h`.
#![allow(dead_code)]

pub const DTV_UNDEFINED: u32 = 0;
pub const DTV_TUNE: u32 = 1;
pub const DTV_CLEAR: u32 = 2;
pub const DTV_FREQUENCY: u32 = 3;
pub const DTV_BANDWIDTH_HZ: u32 = 5;
pub const DTV_INVERSION: u32 = 6;
pub const DTV_DELIVERY_SYSTEM: u32 = 17;
pub const DTV_ISDBT_PARTIAL_RECEPTION: u32 = 18;
pub const DTV_ISDBT_SOUND_BROADCASTING: u32 = 19;
pub const DTV_ISDBT_SB_SUBCHANNEL_ID: u32 = 20;
pub const DTV_ISDBT_SB_SEGMENT_IDX: u32 = 21;
pub const DTV_ISDBT_SB_SEGMENT_COUNT: u32 = 22;
pub const DTV_ISDBT_LAYERA_FEC: u32 = 23;
pub const DTV_ISDBT_LAYERA_MODULATION: u32 = 24;
pub const DTV_ISDBT_LAYERA_SEGMENT_COUNT: u32 = 25;
pub const DTV_ISDBT_LAYERA_TIME_INTERLEAVING: u32 = 26;
pub const DTV_ISDBT_LAYERB_FEC: u32 = 27;
pub const DTV_ISDBT_LAYERB_MODULATION: u32 = 28;
pub const DTV_ISDBT_LAYERB_SEGMENT_COUNT: u32 = 29;
pub const DTV_ISDBT_LAYERB_TIME_INTERLEAVING: u32 = 30;
pub const DTV_ISDBT_LAYERC_FEC: u32 = 31;
pub const DTV_ISDBT_LAYERC_MODULATION: u32 = 32;
pub const DTV_ISDBT_LAYERC_SEGMENT_COUNT: u32 = 33;
pub const DTV_ISDBT_LAYERC_TIME_INTERLEAVING: u32 = 34;
pub const DTV_ISDBT_LAYER_ENABLED: u32 = 41;
pub const DTV_STREAM_ID: u32 = 42;
pub const DTV_GUARD_INTERVAL: u32 = 38;
pub const DTV_TRANSMISSION_MODE: u32 = 39;
pub const DTV_STAT_SIGNAL_STRENGTH: u32 = 62;
pub const DTV_STAT_CNR: u32 = 63;
pub const DTV_STAT_PRE_ERROR_BIT_COUNT: u32 = 64;
pub const DTV_STAT_PRE_TOTAL_BIT_COUNT: u32 = 65;
pub const DTV_STAT_POST_ERROR_BIT_COUNT: u32 = 66;
pub const DTV_STAT_POST_TOTAL_BIT_COUNT: u32 = 67;

/// PID used by `dvbv5-zap --all-pids` (`dvbv5-zap.c`).
pub const MYTHOLOGICAL_FULLMUX_PID: u16 = 0x2000;
