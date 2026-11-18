//! On Windows, embeds an application manifest into `better.exe`. It turns on
//! the UTF-8 code page, so R (which `better --kernel` runs in this process)
//! doesn't fall back to the system code page and mangle non-ASCII text. It
//! also turns on long-path support and declares Windows compatibility.
//!
//! Ark embeds the same manifest into `ark.exe`, but the link argument its
//! build script emits only reaches ark's own binaries, not ours.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=resources/manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // `compile` links into the binary only; the unit tests don't start R.
        embed_resource::compile("resources/manifest/better-manifest.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
