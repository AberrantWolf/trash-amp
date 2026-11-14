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
    fn load_incremental(
        &self,
        source: &str,
        _interactive: bool,
        cancelled: &dyn Fn() -> bool,
        progress: &dyn Fn(amp_app::sources::SourceProgress),
        publish: &dyn Fn(ExternalCatalog),
    ) -> Result<ExternalCatalog> {
        let key = key(self, source)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let snapshot =
            runtime.block_on(self.refresh_catalog(key, cancelled, progress, &|snapshot| {
                if let Ok(data) = catalog(self, key, Some(snapshot.clone()), true) {
                    publish(data);
                }
            }));
        catalog(self, key, Some(snapshot?), true)
    }
    fn search(
        &self,
        id: &str,
        query: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<Library>> {
        let key = key(self, id)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime
            .block_on(self.search_tracks(key, query, cancelled))
            .map(Some)
    }
    fn has_artwork(&self) -> bool {
        true
    }
    fn artwork(
        &self,
        data: &ExternalCatalog,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<BTreeMap<String, String>>> {
        self.artwork_incremental(data, cancelled, &|_| {})
    }
    fn artwork_incremental(
        &self,
        data: &ExternalCatalog,
        cancelled: &dyn Fn() -> bool,
        publish: &dyn Fn(BTreeMap<String, String>),
    ) -> Result<Option<BTreeMap<String, String>>> {
        let key = key(self, &data.source)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime
            .block_on(self.refresh_artwork_incremental(key, cancelled, publish))
            .map(Some)
    }
    fn load(
        &self,
        source: &str,
        interactive: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ExternalCatalog> {
        self.load_with_progress(source, interactive, cancelled, &|_| {})
    }
    fn load_with_progress(
        &self,
        source: &str,
        _: bool,
        cancelled: &dyn Fn() -> bool,
        progress: &dyn Fn(amp_app::sources::SourceProgress),
    ) -> Result<ExternalCatalog> {
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
        let snapshot = runtime.block_on(self.refresh_with_progress(key, cancelled, progress))?;
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
    pub async fn search_tracks(
        &self,
        key: ConnectionKey,
        query: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Library> {
        let client = self.client(key)?;
        let snapshot = self.display_snapshot(key)?.context("Library unavailable")?;
        let mut result = Library {
            id: connection_id(key),
            root: connection_id(key),
            ..Default::default()
        };
        let mut count = 0;
        for collection in snapshot.collections.iter().filter(|c| c.can_browse) {
            let mut cursor = None;
            let mut seen = BTreeSet::new();
            loop {
                let page = remote_read(
                    cancelled,
                    client.songs(
                        collection.id,
                        &api::MusicSearchQuery {
                            search: Some(query.into()),
                            cursor,
                            limit: Some(50),
                        },
                    ),
                )
                .await?;
                for hit in page.items {
                    count += 1;
                    anyhow::ensure!(
                        count <= 5000,
                        "Search has too many results; narrow the query"
                    );
                    let id = hit.song.item_id;
                    let index = result
                        .albums
                        .iter()
                        .position(|a| a.id.item == id.to_string())
                        .unwrap_or_else(|| {
                            result.albums.push(Album {
                                metadata: Default::default(),
                                id: ItemId {
                                    library: connection_id(key),
                                    provider: "collector".into(),
                                    item: id.to_string(),
                                },
                                title: hit.song.album.clone(),
                                artist: hit.song.artist.clone(),
                                tracks: Vec::new(),
                                source_label: collection.name.clone(),
                            });
                            result.albums.len() - 1
                        });
                    for row in hit.sources.iter().filter(|row| row.primary_playback()) {
                        let track = remote_track(key, id, collection.id, &hit.song.artist, row);
                        if !result.albums[index].tracks.iter().any(|t| t.id == track.id) {
                            result.albums[index].tracks.push(track);
                        }
                    }
                }
                cursor = page.next_cursor;
                if let Some(c) = &cursor {
                    anyhow::ensure!(
                        seen.insert(c.clone()) && seen.len() <= 100,
                        "Invalid song pagination"
                    );
                } else {
                    break;
                }
            }
        }
        self.project_library(&mut result, true)?;
        Ok(result)
    }

    pub async fn refresh_artwork(
        &self,
        key: ConnectionKey,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<BTreeMap<String, String>> {
        self.refresh_artwork_incremental(key, cancelled, &|_| {})
            .await
    }
    pub async fn refresh_artwork_incremental(
        &self,
        key: ConnectionKey,
        cancelled: &dyn Fn() -> bool,
        publish: &dyn Fn(BTreeMap<String, String>),
    ) -> Result<BTreeMap<String, String>> {
        let snapshot = self.snapshot(key)?.context("Library unavailable")?;
        let mut failures = 0;
        let mut first_failure = None;
        for (id, (collection, asset, revision)) in &snapshot.artwork_descriptors {
            check(cancelled)?;
            if snapshot
                .artwork
                .get(id)
                .is_some_and(|p| std::path::Path::new(p).is_file())
            {
                continue;
            }
            let result = remote_read(
                cancelled,
                self.client(key)?
                    .thumbnail_at(*collection, *asset, Some(*revision)),
            )
            .await;
            if let Ok(bytes) = result {
                check(cancelled)?;
                let filename = hex::encode(Sha256::digest(
                    format!("{}/{collection}/{asset}/{}", connection_id(key), revision.0)
                        .as_bytes(),
                ));
                let path = self.root.join("art").join(filename);
                self.prune_artwork(bytes.len() as u64)?;
                std::fs::write(&path, bytes)?;
                // Commit each success before publication. Cancellation or a later
                // failed cover cannot discard already downloaded artwork.
                let latest = self.update_snapshot(key, |latest| {
                    check(cancelled)?;
                    if latest.artwork_descriptors.get(id) == snapshot.artwork_descriptors.get(id) {
                        latest
                            .artwork
                            .insert(id.clone(), path.to_string_lossy().into_owned());
                    }
                    Ok(())
                })?;
                check(cancelled)?;
                publish(latest.artwork);
            } else {
                failures += 1;
                if first_failure.is_none() {
                    first_failure = result.err().map(|error| format!("{error:#}"));
                }
            }
        }
        check(cancelled)?;
        if failures > 0 {
            bail!(
                "{failures} covers could not be loaded; titles are available. First failure: {}",
                first_failure.unwrap_or_default()
            );
        }
        Ok(self.snapshot(key)?.context("Library unavailable")?.artwork)
    }

    pub fn project_library(&self, library: &mut Library, online: bool) -> Result<()> {
        self.project_tracks(
            library.albums.iter_mut().flat_map(|a| &mut a.tracks),
            online,
        )
    }

    pub(super) fn project_tracks<'a>(
        &self,
        tracks: impl IntoIterator<Item = &'a mut Track>,
        online: bool,
    ) -> Result<()> {
        let downloaded = self
            .downloads()?
            .into_iter()
            .filter(|d| d.status == crate::DownloadStatus::Complete)
            .map(|d| retained_key(&d.remote))
            .collect::<BTreeSet<_>>();
        for track in tracks {
            let Ok(remote) = parse_remote(track) else {
                continue;
            };
            track.availability = if downloaded.contains(&retained_key(&remote)) {
                amp_core::Availability::Downloaded
            } else if !online {
                amp_core::Availability::Unavailable
            } else {
                crate::playback::online_availability(&remote)
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
            scan_status: None,
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
                    status: crate::DownloadStatus::Complete,
                    error: None,
                    digest: String::new(),
                },
            )
            .unwrap();
        let snapshot = Snapshot {
            library: Library {
                scan: None,
                albums: vec![Album {
                    metadata: Default::default(),
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
            album_revisions: Default::default(),
            track_revisions: Default::default(),
            artwork_descriptors: Default::default(),
            summaries_pending: false,
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
