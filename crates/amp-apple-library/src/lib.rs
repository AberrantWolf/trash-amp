//! Read-only Apple library provider. Native assets never escape this adapter.
#![cfg(any(target_os = "macos", target_os = "ios", test))]
use amp_app::{
    catalog::ExternalCatalog,
    sources::{
        AccessStatus, RefreshKind, SourceChange, SourceDescriptor, SourceObserver, SourceProvider,
    },
};
use amp_audio::{ByteSource, MediaInput, MediaResolver};
use amp_core::{Album, Availability, ItemId, Library, MediaAccess, PlaybackBlockReason, Track};
use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak, atomic::AtomicBool},
};
pub const PROVIDER: &str = "apple-library";
pub const SOURCE: &str = "apple-library:device";
pub struct Adapter {
    root: PathBuf,
    reads: Mutex<Vec<Weak<AtomicBool>>>,
    observation: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}
impl Adapter {
    pub fn open(root: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        Ok(Arc::new(Self {
            root,
            reads: Mutex::default(),
            observation: Mutex::default(),
        }))
    }
}
impl Adapter {
    fn read_cancellation(&self) -> Arc<AtomicBool> {
        let token = Arc::new(AtomicBool::new(false));
        let mut reads = self.reads.lock().unwrap();
        reads.retain(|weak| weak.strong_count() > 0);
        reads.push(Arc::downgrade(&token));
        token
    }
}
/// Native framework calls may block. Cancellation releases the engine's wait
/// without transferring Objective-C objects between unrelated threads.
fn wait_native<T>(rx: &std::sync::mpsc::Receiver<Result<T>>, token: &AtomicBool) -> Result<T> {
    use std::{
        sync::{atomic::Ordering, mpsc::RecvTimeoutError},
        time::Duration,
    };
    loop {
        if token.load(Ordering::Acquire) {
            bail!("Apple audio read cancelled");
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(_) => bail!("Apple audio reader stopped"),
        }
    }
}

#[derive(Clone)]
struct Record {
    id: String,
    album_id: String,
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    disc: u32,
    number: u32,
    duration: f64,
    blocked: Option<PlaybackBlockReason>,
}
fn catalog(records: Vec<Record>, artwork: BTreeMap<String, String>) -> ExternalCatalog {
    let mut albums = BTreeMap::<String, Album>::new();
    for record in records {
        let id = |item| ItemId {
            library: SOURCE.into(),
            provider: PROVIDER.into(),
            item,
        };
        let album = albums
            .entry(record.album_id.clone())
            .or_insert_with(|| Album {
                metadata: Default::default(),
                id: id(record.album_id.clone()),
                title: record.album.clone(),
                artist: record.album_artist.clone(),
                source_label: "Device Music Library".into(),
                tracks: Vec::new(),
            });
        album.tracks.push(Track {
            id: id(record.id.clone()),
            title: record.title,
            artist: record.artist,
            number: record.number,
            disc: record.disc,
            duration_seconds: record.duration,
            availability: if record.blocked.is_none() {
                Availability::Downloaded
            } else {
                Availability::Unavailable
            },
            blocked_reason: record.blocked,
            source: None,
            access: MediaAccess::Managed {
                provider: PROVIDER.into(),
                item: record.id,
            },
        });
    }
    for album in albums.values_mut() {
        album
            .tracks
            .sort_by(|a, b| (a.disc, a.number, &a.id.item).cmp(&(b.disc, b.number, &b.id.item)));
    }
    ExternalCatalog {
        online: None,
        source: SOURCE.into(),
        library: Library {
            id: SOURCE.into(),
            root: String::new(),
            albums: albums.into_values().collect(),
            warnings: Vec::new(),
        },
        artwork,
    }
}
impl SourceProvider for Adapter {
    fn descriptors(&self) -> Result<Vec<SourceDescriptor>> {
        Ok(vec![SourceDescriptor {
            id: SOURCE.into(),
            name: "Device Music Library".into(),
            provider: PROVIDER.into(),
        }])
    }
    fn has_artwork(&self) -> bool {
        true
    }
    fn artwork(
        &self,
        data: &ExternalCatalog,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<BTreeMap<String, String>>> {
        native::artwork(&self.root, data, cancelled).map(Some)
    }
    fn access_status(&self) -> AccessStatus {
        native::access_status()
    }
    fn cache_allowed(&self) -> bool {
        native::authorized()
    }
    fn load(
        &self,
        id: &str,
        interactive: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ExternalCatalog> {
        if id != SOURCE {
            bail!("Unknown Apple library");
        }
        native::authorize(interactive, cancelled)?;
        let data = native::scan(&self.root, cancelled)?;
        if !native::authorized() {
            bail!("Music library access denied");
        }
        Ok(data)
    }
    fn revoke_cached_data(&self) -> Result<()> {
        match std::fs::remove_dir_all(self.root.join("artwork")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    fn observe(&self, changed: SourceObserver) {
        *self.observation.lock().unwrap() = Some(native::observe(Arc::new(move || {
            changed(SourceChange {
                id: SOURCE.into(),
                refresh: RefreshKind::Live,
            })
        })));
    }
}
impl MediaResolver for Adapter {
    fn resolve(&self, track: &Track) -> Result<ByteSource> {
        #[cfg(target_os = "ios")]
        {
            let _ = track;
            bail!("Apple assets require the decoded PCM input");
        }
        #[cfg(not(target_os = "ios"))]
        {
            match native::open(self, track)? {
                MediaInput::Encoded(source) => Ok(source),
                MediaInput::Decoded(_) => bail!("Apple assets require decoded PCM input"),
            }
        }
    }
    fn open(&self, track: &Track) -> Result<MediaInput> {
        native::open(self, track)
    }
    fn cancel(&self) {
        use std::sync::atomic::Ordering;
        self.reads.lock().unwrap().retain(|weak| {
            if let Some(token) = weak.upgrade() {
                token.store(true, Ordering::Release);
                true
            } else {
                false
            }
        });
    }
}
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod native;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
mod native {
    use super::*;
    pub fn authorized() -> bool {
        false
    }
    pub fn access_status() -> AccessStatus {
        AccessStatus::Denied
    }
    pub fn authorize(_: bool, _: &dyn Fn() -> bool) -> Result<()> {
        bail!("Apple music library unavailable on this platform")
    }
    pub fn scan(_: &std::path::Path, _: &dyn Fn() -> bool) -> Result<ExternalCatalog> {
        bail!("Apple music library unavailable")
    }
    pub fn artwork(
        _: &std::path::Path,
        _: &ExternalCatalog,
        _: &dyn Fn() -> bool,
    ) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }
    pub fn open(_: &Adapter, _: &Track) -> Result<MediaInput> {
        bail!("Apple music library unavailable")
    }
    pub fn observe(_: Arc<dyn Fn() + Send + Sync>) -> std::sync::mpsc::Sender<()> {
        std::sync::mpsc::channel().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, album: &str, disc: u32, number: u32) -> Record {
        Record {
            id: id.into(),
            album_id: album.into(),
            title: "Song".into(),
            artist: "Track artist".into(),
            album: "Same album title".into(),
            album_artist: "Various artists".into(),
            disc,
            number,
            duration: 120.0,
            blocked: None,
        }
    }
    #[test]
    fn native_album_ids_preserve_copies_compilations_and_disc_order() {
        let result = catalog(
            vec![
                record("3", "copy-a", 2, 1),
                record("2", "copy-a", 1, 2),
                record("1", "copy-a", 1, 1),
                record("4", "copy-b", 1, 1),
            ],
            BTreeMap::new(),
        );
        assert_eq!(result.library.albums.len(), 2);
        let album = &result.library.albums[0];
        assert_eq!(album.artist, "Various artists");
        assert_eq!(
            album
                .tracks
                .iter()
                .map(|t| t.id.item.as_str())
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        assert!(album.tracks.iter().all(|t| t.artist == "Track artist"));
        assert!(
            matches!(&album.tracks[0].access, MediaAccess::Managed { provider, item } if provider == PROVIDER && item == "1")
        );
        assert!(result.library.root.is_empty());
    }
    #[test]
    fn protected_and_cloud_tracks_remain_browsable_with_specific_reasons() {
        let mut protected = record("1", "album", 1, 1);
        protected.blocked = Some(PlaybackBlockReason::Protected);
        let mut cloud = record("2", "album", 1, 2);
        cloud.blocked = Some(PlaybackBlockReason::NotLocal);
        let result = catalog(
            vec![protected, cloud, record("3", "album", 1, 3)],
            BTreeMap::new(),
        );
        let tracks = &result.library.albums[0].tracks;
        assert_eq!(
            tracks[0].playback_block(),
            Some("Protected music unsupported")
        );
        assert_eq!(tracks[1].playback_block(), Some("Not on this device"));
        assert_eq!(tracks[2].availability, Availability::Downloaded);
    }
    #[test]
    fn native_initialization_wait_can_be_canceled_without_waiting_for_the_native_call() {
        let root = tempfile::tempdir().unwrap();
        let adapter = Adapter::open(root.path().to_owned()).unwrap();
        let token = adapter.read_cancellation();
        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let (entered, waiting) = std::sync::mpsc::channel();
        let worker_token = token.clone();
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            entered.send(()).unwrap();
            let _ = done.send(wait_native(&rx, &worker_token));
        });
        waiting.recv().unwrap();
        adapter.cancel();
        let result = completed.recv_timeout(std::time::Duration::from_secs(2));
        drop(tx);
        worker.join().unwrap();
        assert!(
            result
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }
}
