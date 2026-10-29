use std::{path::PathBuf, rc::Rc, sync::Arc};

fn media_controls(
    window: &slint::Window,
    playback: amp_app::Playback,
) -> Result<amp_ui::MediaSessionGuard, String> {
    #[cfg(target_os = "windows")]
    let hwnd = {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = window.window_handle();
        match handle.window_handle().map_err(|e| e.to_string())?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
            _ => return Err("No Windows media-control window".into()),
        }
    };
    #[cfg(not(target_os = "windows"))]
    let hwnd = {
        let _ = window;
        None
    };
    amp_media::MediaSession::install(
        souvlaki::PlatformConfig {
            dbus_name: "trash_amp",
            display_name: "Trash Amp",
            hwnd,
        },
        playback,
        Arc::new(|task| {
            let _ = slint::invoke_from_event_loop(task);
        }),
    )
    .map(|session| Box::new(session) as amp_ui::MediaSessionGuard)
}

mod drops;

fn main() -> anyhow::Result<()> {
    let data_dir = std::env::var_os("TRASH_AMP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("org", "AberrantWolf", "TrashAmp")
                .expect("Application data directory")
                .data_local_dir()
                .to_owned()
        });
    let service = amp_app::PlaybackService::new(Arc::new(|| Ok(())))?;
    let catalog = amp_app::catalog::CatalogService::open(
        &data_dir.join("library.sqlite3"),
        service.playback(),
    )?;
    let collector = amp_collector::Adapter::open(&data_dir.join("collector"))?;
    amp_ui::run(amp_ui::AppConfig {
        collector,
        open_url: |url, completed| {
            let url = url.to_owned();
            std::thread::spawn(move || {
                completed(webbrowser::open(&url).map_err(|e| e.to_string()))
            });
        },
        args: std::env::args().collect(),
        folder_picker: Some(Rc::new(|selected| {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Open music library")
                .pick_folder()
            {
                selected(path);
            }
        })),
        file_drop: Some(drops::install),
        media_controls,
        playback: service.playback(),
        catalog: catalog.catalog(),
    })
}
