//! Catalog-source adapter. Jobs are owned by amp-app, independently of Slint.
use super::*;
use amp_app::{
    catalog::ExternalCatalog,
    sources::{RefreshKind, SourceChange, SourceDescriptor, SourceObserver, SourceProvider},
};
pub fn source_id(key: ConnectionKey) -> String {
    connection_id(key)
}
fn key(adapter: &Adapter, source: &str) -> Result<ConnectionKey> {
    adapter
        .connections()?
        .into_iter()
        .find(|c| source_id(c.key) == source)
        .map(|c| c.key)
        .context("Collector source unavailable")
}
fn catalog(
    adapter: &Adapter,
    key: ConnectionKey,
    snapshot: Option<Snapshot>,
    online: bool,
) -> Result<ExternalCatalog> {
    let mut library = snapshot
        .as_ref()
        .map(|s| s.library.clone())
        .unwrap_or_default();
    // One cache read per projection rather than one SQLite/JSON scan per song.
    adapter.project_library(&mut library, online)?;
    Ok(ExternalCatalog {
        online: Some(online),
        source: source_id(key),
        library,
        artwork: snapshot.map(|s| s.artwork).unwrap_or_default(),
    })
}
impl SourceProvider for Adapter {
    fn descriptors(&self) -> Result<Vec<SourceDescriptor>> {
        Ok(self
            .connections()?
            .into_iter()
            .map(|c| SourceDescriptor {
                id: source_id(c.key),
                name: c.name,
                provider: "collector".into(),
            })
            .collect())
    }
    fn legacy_selection(&self) -> Result<Option<String>> {
        Ok(self.selected()?.map(source_id))
    }
    fn catalog_cache(&self) -> bool {
        false
    }
    fn cached(&self, source: &str) -> Result<Option<ExternalCatalog>> {
        let key = key(self, source)?;
        catalog(self, key, self.snapshot(key)?, false).map(Some)
    }
    fn project_cached(
        &self,
        source: &str,
        online: Option<bool>,
    ) -> Result<Option<ExternalCatalog>> {
        let key = key(self, source)?;
        let removed = self
            .connections()?
            .iter()
            .any(|c| c.key == key && c.removed);
        catalog(
            self,
            key,
            self.snapshot(key)?,
            online.unwrap_or(false) && !removed,
        )
        .map(Some)
    }
    fn observe(&self, changed: SourceObserver) {
        *self.source_observer.lock().unwrap() = Some(changed);
    }
    fn load(&self, source: &str, _: bool, cancelled: &dyn Fn() -> bool) -> Result<ExternalCatalog> {
        let key = key(self, source)?;
        if self
            .connections()?
            .iter()
            .any(|c| c.key == key && c.removed)
        {
            return catalog(self, key, self.snapshot(key)?, false);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let snapshot = runtime.block_on(self.refresh(key, cancelled))?;
        catalog(self, key, Some(snapshot), true)
    }
}

fn retained_key(remote: &RemoteTrack) -> (String, String, String, String, u16) {
    (
        connection_id(remote.key),
        remote.collection.to_string(),
        remote.capture.to_string(),
        remote.source.to_string(),
        remote.position,
    )
}
impl Adapter {
    pub fn project_library(&self, library: &mut Library, online: bool) -> Result<()> {
        let downloaded = self
            .downloads()?
            .into_iter()
            .filter(|d| d.status == "complete")
            .map(|d| retained_key(&d.remote))
            .collect::<BTreeSet<_>>();
        for track in library.albums.iter_mut().flat_map(|a| &mut a.tracks) {
            let Ok(remote) = parse_remote(track) else {
                continue;
            };
            track.availability = if downloaded.contains(&retained_key(&remote)) {
                amp_core::Availability::Downloaded
            } else if !online {
                amp_core::Availability::Unavailable
            } else {
                match remote.audio.as_ref().map(|audio| &audio.state) {
                    Some(api::MusicTrackAvailabilityState::Available { .. }) => {
                        amp_core::Availability::Available
                    }
                    Some(api::MusicTrackAvailabilityState::Preparing) => {
                        amp_core::Availability::Preparing
                    }
                    _ => amp_core::Availability::Unavailable,
                }
            };
        }
        Ok(())
    }
    pub(super) fn notify_availability(&self, key: ConnectionKey) {
        let observer = self.source_observer.lock().unwrap().clone();
        if let Some(observer) = observer {
            observer(SourceChange {
                id: source_id(key),
                refresh: RefreshKind::Cached,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn online_status_is_explicit_when_every_track_is_downloaded_and_removal_notifies_owner() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = Adapter::open(directory.path()).unwrap();
        let key = ConnectionKey {
            server_id: ids::ServerId::new(),
            user_id: ids::UserId::new(),
        };
        let remote = RemoteTrack {
            key,
            item: ids::ItemId::new(),
            collection: ids::CollectionId::new(),
            source: ids::RepresentationId::new(),
            capture: ids::CaptureId::new(),
            position: 1,
            audio: None,
        };
        let track = Track {
            blocked_reason: None,
            availability: amp_core::Availability::Unavailable,
            source: None,
            id: ItemId {
                library: "library".into(),
                provider: "collector".into(),
                item: "track".into(),
            },
            title: "Track".into(),
            artist: "Artist".into(),
            number: 1,
            disc: 1,
            duration_seconds: 1.0,
            access: MediaAccess::Managed {
                provider: "collector".into(),
                item: serde_json::to_string(&remote).unwrap(),
            },
        };
        adapter
            .put(
                "download",
                "cached",
                &Download {
                    id: "cached".into(),
                    track: track.clone(),
                    remote: remote.clone(),
                    bytes: 0,
                    status: "complete".into(),
                    error: None,
                    digest: String::new(),
                },
            )
            .unwrap();
        let snapshot = Snapshot {
            library: Library {
                albums: vec![Album {
                    id: track.id.clone(),
                    title: "Album".into(),
                    artist: "Artist".into(),
                    source_label: "Collector".into(),
                    tracks: vec![track.clone()],
                }],
                ..Default::default()
            },
            artwork: Default::default(),
            collections: vec![],
            lists: vec![],
            album_collections: Default::default(),
            updated: 0,
        };
        let projected = catalog(&adapter, key, Some(snapshot), true).unwrap();
        assert_eq!(projected.online, Some(true));
        assert_eq!(
            projected.library.albums[0].tracks[0].availability,
            amp_core::Availability::Downloaded
        );
        let (tx, rx) = std::sync::mpsc::channel();
        adapter.observe(Arc::new(move |change| {
            tx.send(change).unwrap();
        }));
        adapter.remove_download(&track).unwrap();
        let change = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(change.id, source_id(key));
        assert_eq!(change.refresh, RefreshKind::Cached);
        let mut library = projected.library;
        adapter.project_library(&mut library, false).unwrap();
        assert_eq!(
            library.albums[0].tracks[0].availability,
            amp_core::Availability::Unavailable
        );
    }
}
