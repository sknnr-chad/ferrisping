//! Gives the Windows executable its icon, and the name Task Manager and Explorer show.
//!
//! The icon is drawn by `examples/make_icons.rs` from `assets/ferrisping*.svg`.

fn main() {
    println!("cargo:rerun-if-changed=assets/ferrisping.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resources = winresource::WindowsResource::new();
        resources
            .set_icon("assets/ferrisping.ico")
            .set("FileDescription", "FerrisPing")
            .set("ProductName", "FerrisPing");
        resources.compile().expect("embed the Windows icon and version info");
    }
}
