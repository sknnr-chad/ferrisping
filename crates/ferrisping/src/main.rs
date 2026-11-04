//! FerrisPing: watch latency, packet loss and outages for a handful of hosts.

// Release builds are GUI-only; debug builds keep a console for panics and logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod add;
mod alerts;
mod app;
mod args;
mod events;
mod format;
mod lanes;
mod route;
mod series;
mod theme;
mod tray;
mod window;

use eframe::egui;
use eframe::egui_wgpu::WgpuSetup;
use eframe::wgpu;

fn main() -> eframe::Result {
    let args = args::Args::parse(std::env::args().skip(1));
    // FerrisPing started normally watches the saved hosts, and one copy is enough:
    // starting it again opens the window of the one that's running, maybe from the
    // tray. Sessions of hosts from the command line run on their own.
    let main_copy = args.hosts.is_empty() && args.screenshot.is_none();
    if main_copy && !tray::first_copy() {
        return Ok(());
    }
    // Pictures for docs go without a tray icon.
    let with_tray = args.screenshot.is_none();
    let (size, corner) = window::first_window(window::work_area());
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("FerrisPing")
        .with_app_id("FerrisPing")
        .with_inner_size(size)
        .with_min_inner_size(window::MIN_SIZE)
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/ferrisping-64.png"))
                .expect("the embedded icon is a valid PNG"),
        );
    if let Some(corner) = corner {
        viewport = viewport.with_position(corner);
    }
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    // Direct3D 12 needs half the memory of Vulkan (wgpu's first pick) on Intel graphics,
    // and OpenGL covers machines without it. A monitor left running all day shouldn't
    // wake a laptop's discrete GPU either. WGPU_BACKEND and WGPU_POWER_PREF override both.
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends =
            wgpu::Backends::from_env().unwrap_or(wgpu::Backends::DX12 | wgpu::Backends::GL);
        setup.power_preference = wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
    }
    if args.screenshot.is_some() {
        // Pictures for docs and checks start from the defaults, and leave the saved
        // window and panel sizes alone.
        options.persist_window = false;
        options.persistence_path = Some(std::env::temp_dir().join("ferrisping-screenshot.ron"));
    }
    eframe::run_native(
        "FerrisPing",
        options,
        Box::new(move |cc| {
            let mut app = app::FerrisPingApp::new(cc, args);
            if with_tray && let Some(tray) = tray::Tray::start(&cc.egui_ctx, main_copy) {
                app.attach_tray(tray);
            }
            Ok(Box::new(app))
        }),
    )
}
