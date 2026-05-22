//! Linux DVB v5 bindings (generated) + raw ioctl helpers.

pub mod dmx_abi;
pub mod ioctl;

pub mod ffi {
    #![allow(
        non_camel_case_types,
        non_snake_case,
        unused,
        clippy::upper_case_acronyms
    )]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use ffi::*;
