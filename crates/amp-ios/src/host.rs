//! iOS launch services. UIKit/file-provider integration belongs in this crate.
use amp_collector::NativeCredentialStorage;
use anyhow::{Context, anyhow};
use objc2::{AnyThread, rc::Retained};
use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategoryPlayback};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use objc2_foundation::{
    NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains,
};
use objc2_media_player::{
    MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
    MPNowPlayingInfoPropertyElapsedPlaybackTime,
};
use objc2_media_player::{MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyPlaybackRate};
use objc2_ui_kit::UIImage;
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

#[derive(Default)]
struct NativeMetadata {
    artwork_path: Option<String>,
    artwork: Option<Retained<MPMediaItemArtwork>>,
}
impl NativeMetadata {
    fn update(&mut self, state: &amp_app::NowPlaying) {
        let path = state
            .album
            .as_ref()
            .and_then(|album| album.artwork_path.as_ref());
        if self.artwork_path.as_ref() != path {
            self.artwork_path = path.cloned();
            // Decode cached provider artwork once per album. Missing/invalid files
            // simply omit artwork; never hand them to Souvlaki's unchecked file reader.
            self.artwork = path.and_then(|path| UIImage::imageWithContentsOfFile(&NSString::from_str(path)))
            .map(|image| {
                let size = unsafe { image.size() };
                let handler = block2::RcBlock::new(move |_: objc2_core_foundation::CGSize| {
                    Retained::as_ptr(&image) as *mut UIImage
                });
                // SAFETY: The copied block retains its UIImage and always returns
                // that valid image. This hook runs on the main dispatch queue.
                unsafe { objc2::msg_send![MPMediaItemArtwork::alloc(), initWithBoundsSize: size, requestHandler: &*handler] }
            });
        }
        // SAFETY: MediaSession dispatches this hook on the main queue. Build the
        // complete state before publishing: separate metadata/progress/artwork
        // writes expose incomplete states to lock-screen scrubbing animations.
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            let Some(track) = &state.track else {
                center.setNowPlayingInfo(None);
                return;
            };
            let info = NSMutableDictionary::<NSString, objc2::runtime::AnyObject>::new();
            info.insert(MPMediaItemPropertyTitle, &*NSString::from_str(&track.title));
            info.insert(
                MPMediaItemPropertyArtist,
                &*NSString::from_str(&track.artist),
            );
            if let Some(album) = &state.album {
                info.insert(
                    MPMediaItemPropertyAlbumTitle,
                    &*NSString::from_str(&album.title),
                );
            }
            info.insert(
                MPMediaItemPropertyPlaybackDuration,
                &*NSNumber::new_f64(track.duration_seconds.max(0.0)),
            );
            info.insert(
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                &*NSNumber::new_f64(state.seconds.max(0.0)),
            );
            info.insert(
                MPNowPlayingInfoPropertyPlaybackRate,
                &*NSNumber::new_f64(if state.playing { 1.0 } else { 0.0 }),
            );
            if let Some(artwork) = &self.artwork {
                info.insert(MPMediaItemPropertyArtwork, &**artwork);
            }
            center.setNowPlayingInfo(Some(&info));
        }
    }
}

fn media_controls(
    _window: &slint::Window,
    playback: amp_app::Playback,
) -> Result<amp_ui::MediaSessionGuard, String> {
    let metadata = std::cell::RefCell::new(NativeMetadata::default());
    amp_media::MediaSession::install_with_native_metadata(
        souvlaki::PlatformConfig {
            dbus_name: "trash_amp",
            display_name: "Trash Amp",
            hwnd: None,
        },
        playback,
        Arc::new(|task| dispatch2::DispatchQueue::main().exec_async(task)),
        Box::new(move |state| metadata.borrow_mut().update(state)),
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
