//! VibeCAD: a small native window over FreeCAD's modelling core.
//!
//! FreeCAD runs headless in its own process (see `worker/vibecad_worker.py`); this program draws
//! its shapes with wgpu and edits its documents through that worker.
//!
//!     vibecad [document.FCStd]
//!     vibecad --mcp        an MCP server on stdio that drives the running window (see agent.rs)

mod agent;
mod app;
mod camera;
mod mcp;
mod modelling;
mod navigation;
mod profile;
mod render;
mod scene;
mod selftest;
mod ui_chrome;
mod worker;

fn main() -> eframe::Result {
    if std::env::args_os().nth(1).is_some_and(|a| a == "--mcp") {
        if let Err(e) = mcp::serve() {
            eprintln!("vibecad --mcp: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let open = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    // The self-test starts from the defaults and leaves the user's settings alone.
    let (profile, settings, problem) =
        if std::env::var_os("VIBECAD_SELFTEST").is_some() { (profile::Profile::off(), Default::default(), None) } else { profile::Profile::load() };
    // Ask the GPU for the buffer sizes it really has: wgpu's defaults stop at 256 MB, which a
    // large assembly passes.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        let base = setup.device_descriptor.clone();
        setup.device_descriptor = std::sync::Arc::new(move |adapter| {
            let mut d = base(adapter);
            let have = adapter.limits();
            d.required_limits.max_buffer_size = have.max_buffer_size;
            // Face colours live in a storage buffer, one per face of every instance.
            d.required_limits.max_storage_buffer_binding_size = have.max_storage_buffer_binding_size;
            d
        });
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size(settings.window_size)
            .with_maximized(settings.maximized)
            .with_title("VibeCAD"),
        ..Default::default()
    };
    eframe::run_native("VibeCAD", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, open, profile, settings, problem)))))
}
