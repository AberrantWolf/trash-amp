//! iOS launch services. UIKit/file-provider integration belongs in this crate.
use amp_collector::NativeCredentialStorage;
use anyhow::{Context, anyhow};
use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategoryPlayback};
use objc2_foundation::{NSMutableDictionary, NSNumber};
use objc2_foundation::{
    NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains,
};
use objc2_media_player::{MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyPlaybackRate};
use std::{path::PathBuf, sync::Arc};

fn open_url(url: &str, completed: amp_ui::UrlOpened) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        completed(Err("Browser requests must run on the main thread".into()));
        return;
    };
    let Some(url) =
        objc2_foundation::NSURL::URLWithString(&objc2_foundation::NSString::from_str(url))
    else {
        completed(Err("Invalid approval address".into()));
        return;
    };
    let completed = std::sync::Mutex::new(Some(completed));
    let completion = block2::RcBlock::new(move |success: objc2::runtime::Bool| {
        if let Some(completed) = completed.lock().unwrap().take() {
            completed(if success.as_bool() {
                Ok(())
            } else {
                Err("iOS could not open the browser".into())
            });
        }
    });
    // SAFETY: UIKit is invoked on the main thread with an empty options dictionary.
    // The copied completion block reports failures through the controller's channel.
    unsafe {
        objc2_ui_kit::UIApplication::sharedApplication(mtm).openURL_options_completionHandler(
            &url,
            &objc2_foundation::NSDictionary::new(),
            Some(&completion),
        );
    }
}

fn update_playback_rate(state: &amp_app::NowPlaying) {
    // SAFETY: MediaSession calls this on the main queue, after Souvlaki has
    // replaced its metadata. iOS extrapolates elapsed time using this rate;
    // MPNowPlayingInfoCenter.playbackState only controls the macOS clock.
    unsafe {
        let center = MPNowPlayingInfoCenter::defaultCenter();
        if state.track.is_none() {
            center.setNowPlayingInfo(None);
            return;
        }
        if let Some(info) = center.nowPlayingInfo() {
            let info = NSMutableDictionary::dictionaryWithDictionary(&info);
            let rate = NSNumber::new_f64(if state.playing { 1.0 } else { 0.0 });
            info.insert(MPNowPlayingInfoPropertyPlaybackRate, &*rate);
            center.setNowPlayingInfo(Some(&info));
        }
    }
}

fn media_controls(
    _window: &slint::Window,
    playback: amp_app::Playback,
) -> Result<amp_ui::MediaSessionGuard, String> {
    amp_media::MediaSession::install_with_update(
        souvlaki::PlatformConfig {
            dbus_name: "trash_amp",
            display_name: "Trash Amp",
            hwnd: None,
        },
        playback,
        Arc::new(|task| dispatch2::DispatchQueue::main().exec_async(task)),
        Box::new(update_playback_rate),
    )
    .map(|session| Box::new(session) as amp_ui::MediaSessionGuard)
}

fn prepare_playback() -> anyhow::Result<()> {
    // SAFETY: The service serializes this policy on its worker. Activation is
    // blocking, so it must not run in a UI or audio render callback.
    unsafe {
        let session = AVAudioSession::sharedInstance();
        session
            .setCategory_error(
                AVAudioSessionCategoryPlayback.context("Playback audio category unavailable")?,
            )
            .map_err(|e| anyhow!("Cannot configure music playback: {e}"))?;
        session
            .setActive_error(true)
            .map_err(|e| anyhow!("Cannot activate music playback: {e}"))
    }
}

pub fn run() -> anyhow::Result<()> {
    let paths = NSSearchPathForDirectoriesInDomains(
        NSSearchPathDirectory::ApplicationSupportDirectory,
        NSSearchPathDomainMask::UserDomainMask,
        true,
    );
    let root = paths
        .firstObject()
        .context("Application Support directory unavailable")?;
    let data_dir = PathBuf::from(root.to_string()).join("TrashAmp");
    let credentials = Arc::new(crate::credentials::Credentials);
    let collector =
        amp_collector::Adapter::with_credentials(&data_dir.join("collector"), credentials.clone())?;
    // Upgrade existing WhenUnlocked items while launching in the foreground.
    // SecItemUpdate changes protection on the same service/account item.
    for connection in collector.connections()? {
        if let Some(credential) = credentials.load(connection.key)? {
            credentials.save(&credential)?;
        }
    }
    let service = amp_app::PlaybackService::new(Arc::new(prepare_playback))?;
    let catalog = amp_app::catalog::CatalogService::open(
        &data_dir.join("library.sqlite3"),
        service.playback(),
    )?;
    amp_ui::run(amp_ui::AppConfig {
        collector,
        open_url,
        args: std::env::args().collect(),
        // Collector streaming/downloads are the initial mobile source. External
        // folders need a document picker and security-scoped access, not raw paths.
        folder_picker: None,
        media_controls,
        playback: service.playback(),
        catalog: catalog.catalog(),
    })
}
