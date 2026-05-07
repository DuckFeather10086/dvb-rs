fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rustc-link-lib=c");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_type("fe_status")
        .allowlist_type("fe_status_t")
        .allowlist_type("fe_delivery_system")
        .allowlist_type("fe_code_rate")
        .allowlist_type("fe_modulation")
        .allowlist_type("fe_bandwidth")
        .allowlist_type("fe_guard_interval")
        .allowlist_type("fe_transmit_mode")
        .allowlist_type("fe_hierarchy")
        .allowlist_type("fe_pilot")
        .allowlist_type("fe_rolloff")
        .allowlist_type("fe_spectral_inversion")
        .allowlist_type("dtv_property")
        .allowlist_type("dtv_properties")
        .allowlist_type("dtv_fe_stats")
        .allowlist_type("dtv_stats")
        .allowlist_type("dvb_frontend_info")
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        .derive_default(true)
        .derive_debug(true)
        .layout_tests(false)
        .prepend_enum_name(false)
        .generate()
        .expect("bindgen frontend.h");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings.write_to_file(&out).expect("write bindings");
}
