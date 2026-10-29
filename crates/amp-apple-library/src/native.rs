//! All Objective-C calls use public SDK selectors and remain on their owning worker.
use super::*;
use anyhow::Context;
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject},
};
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use std::{path::Path, time::Duration};
#[cfg(target_os = "macos")]
#[link(name = "iTunesLibrary", kind = "framework")]
unsafe extern "C" {
    static ITLibraryDidChangeNotification: *const NSString;
}
#[cfg(target_os = "macos")]
#[link(name = "StoreKit", kind = "framework")]
unsafe extern "C" {}
#[cfg(target_os = "ios")]
#[link(name = "MediaPlayer", kind = "framework")]
unsafe extern "C" {
    static MPMediaLibraryDidChangeNotification: *const NSString;
}

fn class(name: &std::ffi::CStr) -> &'static AnyClass {
    AnyClass::get(name).expect("Linked Apple framework class")
}
fn permission_class() -> &'static AnyClass {
    // StoreKit exposes the public Objective-C music authorization bridge on macOS.
    // It is deprecated in favor of Swift MusicAuthorization, but needs no subscription.
    #[cfg(target_os = "macos")]
    {
        class(c"SKCloudServiceController")
    }
    #[cfg(target_os = "ios")]
    {
        class(c"MPMediaLibrary")
    }
}
pub fn access_status() -> AccessStatus {
    match unsafe {
        let status: isize = msg_send![permission_class(), authorizationStatus];
        status
    } {
        3 => AccessStatus::Authorized,
        2 => AccessStatus::Restricted,
        1 => AccessStatus::Denied,
        _ => AccessStatus::NotDetermined,
    }
}
pub fn authorized() -> bool {
    unsafe {
        let status: isize = msg_send![permission_class(), authorizationStatus];
        status == 3
    }
}
pub fn authorize(interactive: bool, cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        bail!("Music library operation cancelled");
    }
    let mut status: isize = unsafe { msg_send![permission_class(), authorizationStatus] };
    if status == 3 {
        return Ok(());
    }
    if status == 0 && interactive {
        let (tx, rx) = std::sync::mpsc::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let block = block2::RcBlock::new(move |value: isize| {
                let _ = tx.send(value);
            });
            unsafe {
                let _: () = msg_send![permission_class(), requestAuthorization: &*block];
            }
        });
        loop {
            if cancelled() {
                bail!("Music library operation cancelled");
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(3) => return Ok(()),
                Ok(value) => {
                    status = value;
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    if status == 2 {
        bail!("Music library access is restricted on this device.");
    }
    if status == 1 {
        bail!("Music library access denied. Enable Music access for Trash Amp in Settings.");
    }
    bail!(
        "Music library access is required. Select Device Music Library to grant access, or enable Music access for Trash Amp in Settings."
    )
}
unsafe fn text(object: &AnyObject, selector: objc2::runtime::Sel) -> String {
    let value: Option<Retained<NSString>> =
        unsafe { objc2::msg_send![object, performSelector: selector] };
    value.map(|v| v.to_string()).unwrap_or_default()
}
#[cfg(target_os = "macos")]
unsafe fn object_text(object: &AnyObject, selector: objc2::runtime::Sel) -> String {
    let value: Option<Retained<AnyObject>> =
        unsafe { msg_send![object, performSelector: selector] };
    if let Some(value) = value {
        unsafe { text(&value, objc2::sel!(stringValue)) }
    } else {
        String::new()
    }
}
fn fallback(value: String, default: &str) -> String {
    if value.is_empty() {
        default.into()
    } else {
        value
    }
}
fn check_track(track: &Track) -> Result<&str> {
    if let Some(reason) = track.playback_block() {
        bail!("{reason}");
    }
    match &track.access {
        MediaAccess::Managed { provider, item } if provider == PROVIDER => Ok(item),
        _ => bail!("Apple music reference required"),
    }
}
#[cfg(target_os = "macos")]
fn library() -> Result<Retained<AnyObject>> {
    let mut error: Option<Retained<NSError>> = None;
    let library: Option<Retained<AnyObject>> = unsafe {
        msg_send![class(c"ITLibrary"), libraryWithAPIVersion: &*NSString::from_str("1.0"), error: &mut error]
    };
    library.ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot read Music library: {}",
            error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "access unavailable (use a signed app bundle)".into())
        )
    })
}
#[cfg(target_os = "macos")]
pub fn scan(root: &Path, cancelled: &dyn Fn() -> bool) -> Result<ExternalCatalog> {
    if cancelled() {
        bail!("Music library operation cancelled");
    }
    objc2::rc::autoreleasepool(|_| unsafe {
        let library = library()?;
        let items: Retained<NSArray<AnyObject>> = msg_send![&*library, allMediaItems];
        let mut records = Vec::new();
        let artwork = BTreeMap::new();
        for item in items.to_vec() {
            if cancelled() {
                bail!("Music library operation cancelled");
            }
            let kind: usize = msg_send![&*item, mediaKind];
            if kind != 2 {
                continue;
            } // ITLibMediaItemMediaKindSong
            let id = object_text(&item, objc2::sel!(persistentID));
            let album: Retained<AnyObject> = msg_send![&*item, album];
            let album_id = object_text(&album, objc2::sel!(persistentID));
            let artist: Option<Retained<AnyObject>> = msg_send![&*item, artist];
            let artist = fallback(
                artist
                    .map(|a| text(&a, objc2::sel!(name)))
                    .unwrap_or_default(),
                "Unknown artist",
            );
            let compilation: bool = msg_send![&*album, isCompilation];
            let protected: bool = msg_send![&*item, isDRMProtected];
            let location: Option<Retained<NSURL>> = msg_send![&*item, location];
            let local = location.as_ref().is_some_and(|url| {
                if !url.isFileURL() {
                    return false;
                }
                let scope = SecurityScope {
                    url: url.clone(),
                    scoped: url.startAccessingSecurityScopedResource(),
                };
                scope
                    .url
                    .path()
                    .is_some_and(|p| Path::new(&p.to_string()).is_file())
            });
            let duration: usize = msg_send![&*item, totalTime];
            let number: usize = msg_send![&*item, trackNumber];
            let disc: usize = msg_send![&*album, discNumber];
            records.push(Record {
                album_id: if album_id.is_empty() || album_id == "0" {
                    format!("item:{id}")
                } else {
                    album_id
                },
                title: fallback(text(&item, objc2::sel!(title)), "Untitled"),
                id,
                album: fallback(text(&album, objc2::sel!(title)), "Unknown album"),
                album_artist: fallback(
                    text(&album, objc2::sel!(albumArtist)),
                    if compilation {
                        "Various artists"
                    } else {
                        &artist
                    },
                ),
                artist,
                number: number as u32,
                disc: disc as u32,
                duration: duration as f64 / 1000.0,
                blocked: if protected {
                    Some(PlaybackBlockReason::Protected)
                } else if !local {
                    Some(PlaybackBlockReason::NotLocal)
                } else {
                    None
                },
            });
        }
        let _ = root;
        Ok(catalog(records, artwork))
    })
}
#[cfg(target_os = "macos")]
fn resolve_file(
    track: &Track,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<ByteSource> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        bail!("Apple audio read cancelled");
    }
    let id = check_track(track)?;
    if !authorized() {
        bail!("Music library access denied");
    }
    objc2::rc::autoreleasepool(|_| unsafe {
        let library = library()?;
        let items: Retained<NSArray<AnyObject>> = msg_send![&*library, allMediaItems];
        for item in items.to_vec() {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                bail!("Apple audio read cancelled");
            }
            if object_text(&item, objc2::sel!(persistentID)) != id {
                continue;
            }
            let protected: bool = msg_send![&*item, isDRMProtected];
            if protected {
                bail!("Protected music unsupported");
            }
            let url: Option<Retained<NSURL>> = msg_send![&*item, location];
            let url = url.context("Music file location missing")?;
            if !url.isFileURL() {
                bail!("Not on this device");
            }
            let scoped = url.startAccessingSecurityScopedResource();
            let scope = SecurityScope { url, scoped };
            let result = (|| {
                let path = scope
                    .url
                    .path()
                    .context("Music file location missing")?
                    .to_string();
                let file = std::fs::File::open(&path)?;
                let length = file.metadata()?.len();
                let ext = Path::new(&path).extension().and_then(|v| v.to_str());
                Ok(ByteSource::new(
                    ScopedFile {
                        file,
                        _scope: scope,
                        cancelled: cancelled.clone(),
                    },
                    amp_audio::MediaHint::new(Some(length), ext, None),
                ))
            })();
            return result;
        }
        bail!("Music item no longer available")
    })
}
#[cfg(target_os = "macos")]
struct ScopedFile {
    file: std::fs::File,
    _scope: SecurityScope,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(target_os = "macos")]
struct SecurityScope {
    url: Retained<NSURL>,
    scoped: bool,
}
#[cfg(target_os = "macos")]
impl std::io::Read for ScopedFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::other("Playback cancelled"));
        }
        std::io::Read::read(&mut self.file, buf)
    }
}
#[cfg(target_os = "macos")]
impl std::io::Seek for ScopedFile {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::other("Playback cancelled"));
        }
        std::io::Seek::seek(&mut self.file, position)
    }
}
#[cfg(target_os = "macos")]
impl Drop for SecurityScope {
    fn drop(&mut self) {
        if self.scoped {
            unsafe {
                self.url.stopAccessingSecurityScopedResource();
            }
        }
    }
}
#[cfg(target_os = "macos")]
pub fn open(adapter: &Adapter, track: &Track) -> Result<MediaInput> {
    let token = adapter.read_cancellation();
    let track = track.clone();
    let worker_token = token.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("amp-apple-file".into())
        .spawn(move || {
            let _ = tx.send(resolve_file(&track, &worker_token));
        })?;
    wait_native(&rx, &token).map(MediaInput::Encoded)
}

#[cfg(any(target_os = "ios", all(test, target_os = "macos")))]
mod asset;
#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "ios")]
pub use ios::{open, scan};
/// The observer thread owns native tokens; dropping the adapter disconnects stop.
pub fn observe(changed: Arc<dyn Fn() + Send + Sync>) -> std::sync::mpsc::Sender<()> {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        objc2::rc::autoreleasepool(|_| unsafe {
            let center: Retained<AnyObject> =
                msg_send![class(c"NSNotificationCenter"), defaultCenter];
            #[cfg(target_os = "macos")]
            let distributed: Retained<AnyObject> =
                msg_send![class(c"NSDistributedNotificationCenter"), defaultCenter];
            #[cfg(target_os = "macos")]
            let notifications = [
                (&*distributed, &*ITLibraryDidChangeNotification, 0),
                (
                    &*center,
                    &*NSString::from_str("NSApplicationDidBecomeActiveNotification"),
                    1,
                ),
            ];
            #[cfg(target_os = "ios")]
            let notifications = [
                (&*center, &*MPMediaLibraryDidChangeNotification, 0),
                (
                    &*center,
                    &*NSString::from_str("UIApplicationDidBecomeActiveNotification"),
                    1,
                ),
                (
                    &*center,
                    &*NSString::from_str("UIApplicationDidEnterBackgroundNotification"),
                    2,
                ),
            ];
            let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let mut tokens = Vec::new();
            for (notification_center, name, event) in notifications {
                let active = active.clone();
                let changed = changed.clone();
                let block = block2::RcBlock::new(
                    move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                        if event == 2 {
                            active.store(false, std::sync::atomic::Ordering::Release);
                            return;
                        }
                        if event == 1 {
                            active.store(true, std::sync::atomic::Ordering::Release);
                        }
                        if active.load(std::sync::atomic::Ordering::Acquire) {
                            changed();
                        }
                    },
                );
                let token: Retained<AnyObject> = msg_send![notification_center,
                addObserverForName: name, object: None::<&AnyObject>,
                queue: None::<&AnyObject>, usingBlock: &*block];
                tokens.push((notification_center, token));
            }
            #[cfg(target_os = "ios")]
            {
                let library: Retained<AnyObject> =
                    msg_send![class(c"MPMediaLibrary"), defaultMediaLibrary];
                let _: () = msg_send![&*library, beginGeneratingLibraryChangeNotifications];
            }
            let _ = stopped.recv();
            for (notification_center, token) in tokens {
                let _: () = msg_send![notification_center, removeObserver: &*token];
            }
            #[cfg(target_os = "ios")]
            {
                let library: Retained<AnyObject> =
                    msg_send![class(c"MPMediaLibrary"), defaultMediaLibrary];
                let _: () = msg_send![&*library, endGeneratingLibraryChangeNotifications];
            }
        })
    });
    stop
}

fn save_artwork(root: &Path, id: &str, data: &objc2_foundation::NSData) -> Result<String> {
    use sha2::{Digest, Sha256};
    if !authorized() {
        bail!("Music library access denied");
    }
    if data.len() > 16 * 1024 * 1024 {
        bail!("Artwork too large");
    }
    let bytes = data.to_vec();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.thumbnail(512, 512);
    let directory = root.join("artwork");
    std::fs::create_dir_all(&directory)?;
    let name: String = id
        .bytes()
        .flat_map(|b| format!("{b:02x}").into_bytes())
        .map(char::from)
        .collect();
    // Content-addressed paths keep a canceled older artwork job from replacing
    // bytes referenced by a newer accepted catalog or the current Now Playing.
    let path = directory.join(format!("{name}-{digest}.png"));
    static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = directory.join(format!(
        "{name}-{digest}-{}-{nonce}.tmp",
        std::process::id()
    ));
    image.save_with_format(&temporary, image::ImageFormat::Png)?;
    if !authorized() {
        let _ = std::fs::remove_file(&temporary);
        bail!("Music library access denied");
    }
    std::fs::rename(temporary, &path)?;
    Ok(path.to_string_lossy().into_owned())
}
#[cfg(target_os = "macos")]
pub fn artwork(
    root: &Path,
    data: &ExternalCatalog,
    cancelled: &dyn Fn() -> bool,
) -> Result<BTreeMap<String, String>> {
    if cancelled() || !authorized() {
        return Ok(BTreeMap::new());
    }
    objc2::rc::autoreleasepool(|_| unsafe {
        let library = library()?;
        let items: Retained<NSArray<AnyObject>> = msg_send![&*library, allMediaItems];
        let album_ids: std::collections::BTreeSet<_> = data
            .library
            .albums
            .iter()
            .map(|a| a.id.item.as_str())
            .collect();
        let mut artwork = BTreeMap::new();
        for item in items.to_vec() {
            if cancelled() || !authorized() {
                break;
            }
            let kind: usize = msg_send![&*item, mediaKind];
            if kind != 2 {
                continue;
            }
            let album: Retained<AnyObject> = msg_send![&*item, album];
            let id = object_text(&album, objc2::sel!(persistentID));
            if !album_ids.contains(id.as_str()) || artwork.contains_key(&id) {
                continue;
            }
            let art: Option<Retained<AnyObject>> = msg_send![&*item, artwork];
            if let Some(art) = art {
                let bytes: Option<Retained<objc2_foundation::NSData>> = msg_send![&*art, imageData];
                if let Some(bytes) = bytes
                    && let Ok(path) = save_artwork(root, &id, &bytes)
                {
                    artwork.insert(id, path);
                }
            }
        }
        Ok(artwork)
    })
}
#[cfg(target_os = "ios")]
pub use ios::artwork;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn linked_frameworks_and_permission_status_are_available_without_a_prompt() {
        assert!(AnyClass::get(c"ITLibrary").is_some());
        assert!(AnyClass::get(c"SKCloudServiceController").is_some());
        let _status = access_status();
    }
    #[test]
    fn mac_file_reader_cancellation_is_terminal_and_releases_its_scope() {
        objc2::rc::autoreleasepool(|_| {
            use std::io::{Read, Seek};
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("local");
            std::fs::write(&path, [1, 2, 3]).unwrap();
            let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
            let token = Arc::new(AtomicBool::new(true));
            let mut reader = ScopedFile {
                file: std::fs::File::open(&path).unwrap(),
                _scope: SecurityScope { url, scoped: false },
                cancelled: token,
            };
            assert_eq!(
                reader.read_exact(&mut [0; 1]).unwrap_err().kind(),
                std::io::ErrorKind::Other
            );
            assert_eq!(
                reader.seek(std::io::SeekFrom::Start(0)).unwrap_err().kind(),
                std::io::ErrorKind::Other
            );
        });
    }
}
