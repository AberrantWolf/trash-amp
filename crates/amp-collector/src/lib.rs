//! Collector transport, local persistence and media access. No UI dependencies.
use amp_core::{Album, ItemId, Library, MediaAccess, Track};
use anyhow::{Context, Result, bail};
pub use collector_client::{
    ConnectionKey, EnrolledCredential, NativeCredentialStorage, ServerUrl,
    collector_api_types as api, collector_core as ids,
};
use collector_client::{EnrolledClient, MediaCancellation, Pairing, PendingEnrollment};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct Keychain;
impl Keychain {
    fn entry(key: ConnectionKey) -> ids::Result<keyring::Entry> {
        keyring::Entry::new(
            "trash-amp.collector",
            &format!("{}/{}", key.server_id, key.user_id),
        )
        .map_err(|_| ids::Error::Unavailable("Keychain unavailable".into()))
    }
}
impl NativeCredentialStorage for Keychain {
    fn save(&self, value: &EnrolledCredential) -> ids::Result<()> {
        let key = ConnectionKey {
            server_id: value.server_id,
            user_id: value.user_id.ok_or(ids::Error::Unauthenticated)?,
        };
        Self::entry(key)?
            .set_password(
                &serde_json::to_string(value)
                    .map_err(|_| ids::Error::Invalid("Invalid credential".into()))?,
            )
            .map_err(|_| ids::Error::Unavailable("Could not save credential in Keychain".into()))
    }
    fn load(&self, key: ConnectionKey) -> ids::Result<Option<EnrolledCredential>> {
        match Self::entry(key)?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| ids::Error::Invalid("Invalid Keychain credential".into())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(ids::Error::Unavailable("Could not read Keychain".into())),
        }
    }
    fn remove(&self, key: ConnectionKey) -> ids::Result<()> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(ids::Error::Unavailable(
                "Could not remove Keychain credential".into(),
            )),
        }
    }
    fn load_legacy(&self, _: ids::ServerId) -> ids::Result<Option<EnrolledCredential>> {
        Ok(None)
    }
    fn remove_legacy(&self, _: ids::ServerId) -> ids::Result<()> {
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedConnection {
    pub key: ConnectionKey,
    pub name: String,
    pub endpoint: String,
    pub removed: bool,
}
/// Verified media capability for the selected server, independent of cached audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connected,
    Public,
    Lan,
    Direct,
    StreamingUnavailable,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub library: Library,
    pub collections: Vec<api::ClientCollection>,
    pub lists: Vec<api::CollectionListView>,
    pub artwork: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub album_collections: std::collections::BTreeMap<String, ids::CollectionId>,
    pub updated: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteTrack {
    pub key: ConnectionKey,
    pub item: ids::ItemId,
    pub collection: ids::CollectionId,
    pub source: ids::RepresentationId,
    pub capture: ids::CaptureId,
    pub position: u16,
    pub audio: Option<api::MusicTrackAvailability>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    pub track: Track,
    pub remote: RemoteTrack,
    pub bytes: u64,
    pub status: String,
    pub error: Option<String>,
    pub digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ListCommand {
    pub key: ConnectionKey,
    pub collection: ids::CollectionId,
    pub list: Option<ids::CollectionListId>,
    pub create: Option<api::CreateCollectionList>,
    pub update: Option<api::UpdateCollectionList>,
    pub receipt: String,
}
pub struct Adapter {
    _profile_lock: std::fs::File,
    secrets: Arc<dyn NativeCredentialStorage>,
    root: PathBuf,
    db: Mutex<Connection>,
    media: Mutex<Vec<(ConnectionKey, MediaCancellation)>>,
    downloads_cancel: AtomicBool,
    download_token: Mutex<Option<MediaCancellation>>,
}
fn connection_id(key: ConnectionKey) -> String {
    format!("{}/{}", key.server_id, key.user_id)
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn check(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        bail!("Cancelled")
    }
    Ok(())
}
impl Adapter {
    pub fn open(root: &Path) -> Result<Arc<Self>> {
        Self::with_credentials(root, Arc::new(Keychain))
    }
    pub fn with_credentials(
        root: &Path,
        secrets: Arc<dyn NativeCredentialStorage>,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(root.join("audio"))?;
        std::fs::create_dir_all(root.join("art"))?;
        let profile_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("profile.lock"))?;
        fs2::FileExt::try_lock_exclusive(&profile_lock)
            .context("This Trash Amp profile is already open in another process")?;
        let db = Connection::open(root.join("collector.sqlite3"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS records(kind TEXT, id TEXT, value TEXT, PRIMARY KEY(kind,id));")?;
        Ok(Arc::new(Self {
            _profile_lock: profile_lock,
            secrets,
            root: root.to_owned(),
            db: Mutex::new(db),
            media: Mutex::new(vec![]),
            downloads_cancel: AtomicBool::new(false),
            download_token: Mutex::new(None),
        }))
    }
    fn put(&self, kind: &str, id: &str, value: &impl Serialize) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO records VALUES (?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value", params![kind,id,serde_json::to_string(value)?])?;
        Ok(())
    }
    fn get<T: serde::de::DeserializeOwned>(&self, kind: &str, id: &str) -> Result<Option<T>> {
        use rusqlite::OptionalExtension;
        let value: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM records WHERE kind=?1 AND id=?2",
                params![kind, id],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    fn all<T: serde::de::DeserializeOwned>(&self, kind: &str) -> Result<Vec<T>> {
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare("SELECT value FROM records WHERE kind=? ORDER BY id")?;
        let rows = statement.query_map([kind], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    fn prune_artwork(&self, incoming: u64) -> Result<()> {
        let mut entries = std::fs::read_dir(self.root.join("art"))?
            .map(|e| {
                let e = e?;
                let m = e.metadata()?;
                Ok((m.modified().ok(), m.len(), e.path()))
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.0);
        let mut total = entries.iter().map(|e| e.1).sum::<u64>() + incoming;
        for (_, size, path) in entries {
            if total <= 128 * 1024 * 1024 {
                break;
            }
            std::fs::remove_file(path)?;
            total = total.saturating_sub(size);
        }
        Ok(())
    }
    pub fn connections(&self) -> Result<Vec<SavedConnection>> {
        self.all("connection")
    }
    pub fn selected(&self) -> Result<Option<ConnectionKey>> {
        Ok(self
            .get::<Option<ConnectionKey>>("setting", "selected")?
            .flatten())
    }
    pub fn select(&self, key: Option<ConnectionKey>) -> Result<()> {
        self.put("setting", "selected", &key)
    }
    pub fn snapshot(&self, key: ConnectionKey) -> Result<Option<Snapshot>> {
        self.get("snapshot", &connection_id(key))
    }
    pub fn save_connection(
        &self,
        credential: &EnrolledCredential,
        endpoint: String,
    ) -> Result<ConnectionKey> {
        self.secrets.save(credential)?;
        let key = ConnectionKey {
            server_id: credential.server_id,
            user_id: credential.user_id.context("Missing account identity")?,
        };
        self.put(
            "connection",
            &connection_id(key),
            &SavedConnection {
                key,
                name: format!("{} · {}", endpoint, credential.display_name),
                endpoint,
                removed: false,
            },
        )?;
        Ok(key)
    }
    pub fn remove(&self, key: ConnectionKey) -> Result<()> {
        self.secrets.remove(key)?;
        for (connection, token) in self.media.lock().unwrap().iter() {
            if *connection == key {
                token.cancel()
            }
        }
        let mut c: SavedConnection = self
            .get("connection", &connection_id(key))?
            .context("Unknown connection")?;
        c.removed = true;
        c.name = format!("{} (offline)", c.name);
        self.put("connection", &connection_id(key), &c)
    }
    pub fn client(&self, key: ConnectionKey) -> Result<EnrolledClient> {
        Ok(EnrolledClient::from_storage(self.secrets.as_ref(), key)?)
    }
    /// Run on a worker. Uses the shared client's trust and fallback policy and a
    /// media HEAD request; no audio is downloaded or prepared by this probe.
    pub fn connection_state(&self, key: ConnectionKey) -> ConnectionState {
        let Ok(client) = self.client(key) else {
            return ConnectionState::Disconnected;
        };
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return ConnectionState::Disconnected;
        };
        if runtime.block_on(client.collections()).is_err() {
            return ConnectionState::Disconnected;
        }
        let Some(snapshot) = self.snapshot(key).ok().flatten() else {
            return ConnectionState::Connected;
        };
        let candidate = snapshot
            .library
            .albums
            .iter()
            .flat_map(|a| &a.tracks)
            .filter_map(|track| parse_remote(track).ok())
            .find_map(|remote| match remote.audio.as_ref().map(|a| &a.state) {
                Some(api::MusicTrackAvailabilityState::Available {
                    representation_id,
                    file_index,
                    ..
                }) => Some((remote.item, *representation_id, *file_index)),
                _ => None,
            });
        let Some((item, representation, file)) = candidate else {
            return ConnectionState::Connected;
        };
        match client.media_reader(item, representation, file, MediaCancellation::default()) {
            Ok(reader) if reader.is_local() => ConnectionState::Lan,
            Ok(_) => runtime
                .block_on(async {
                    // RangeReader exposes only is_local(). The shared client's
                    // nonlocal policy selects advertised direct HTTPS before public
                    // fallback, and fails if that direct endpoint cannot be verified.
                    // Classify only after a successful media HEAD, never from an
                    // advertisement alone. An unreadable descriptor leaves the label
                    // unconfirmed rather than guessing which nonlocal route worked.
                    let mut response = client
                        .get(client.api_url("api/v1/playback/routes")?)?
                        .send()
                        .await?
                        .error_for_status()?;
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await? {
                        if bytes.len() + chunk.len() > 128 * 1024 {
                            bail!("Oversized playback descriptor")
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    let descriptor: api::LocalPlaybackDescriptor = serde_json::from_slice(&bytes)?;
                    if descriptor.server_id != key.server_id {
                        bail!("Playback server identity mismatch")
                    }
                    Ok::<_, anyhow::Error>(if descriptor.remote_media.is_some() {
                        ConnectionState::Direct
                    } else if descriptor.remote_media_allowed {
                        ConnectionState::Public
                    } else {
                        ConnectionState::Connected
                    })
                })
                .unwrap_or(ConnectionState::Connected),
            Err(_) => ConnectionState::StreamingUnavailable,
        }
    }
    pub async fn enroll(
        &self,
        url: ServerUrl,
        trust: Option<String>,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(String),
    ) -> Result<ConnectionKey> {
        let endpoint = url.to_string();
        let entry = keyring::Entry::new("trash-amp.collector", "enrollment")?;
        #[derive(Serialize, Deserialize)]
        struct Attempt {
            endpoint: String,
            pending: PendingEnrollment,
            completed: Option<EnrolledCredential>,
        }
        let saved = match entry.get_password() {
            Ok(raw) => Some(serde_json::from_str::<Attempt>(&raw)?),
            Err(keyring::Error::NoEntry) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(attempt) = saved.as_ref().filter(|a| a.endpoint == endpoint)
            && let Some(credential) = &attempt.completed
        {
            let key = self.save_connection(credential, endpoint)?;
            entry.delete_credential()?;
            return Ok(key);
        }
        let pending = if let Some(attempt) = saved.filter(|a| a.endpoint == endpoint) {
            attempt.pending
        } else {
            if let Some(material) = trust {
                let mut pairing: Pairing =
                    serde_json::from_str(&material).unwrap_or_else(|_| Pairing {
                        server_id: ids::ServerId::new(),
                        certificate_sha256: ids::Digest::sha256(material.as_bytes()).hex,
                        certificate_pem: material,
                        endpoints: vec![],
                    });
                let client = pairing.lan_client()?;
                let mut pending = PendingEnrollment::start(&client, url, "Trash Amp").await?;
                pairing.server_id = pending.server_id();
                pending.pin(pairing)?;
                pending
            } else {
                let client = collector_client::system_https_client()?;
                PendingEnrollment::start_system_https(&client, url, "Trash Amp").await?
            }
        };
        let client = pending.client()?;
        entry.set_password(&serde_json::to_string(&Attempt {
            endpoint: endpoint.clone(),
            pending: pending.clone(),
            completed: None,
        })?)?;
        progress(pending.start.verification_url.clone());
        loop {
            if cancelled() {
                let _ = pending.status(&client, true).await;
                let _ = entry.delete_credential();
                bail!("Enrollment cancelled")
            }
            let status = pending.status(&client, false).await?;
            match status.phase {
                api::EnrollmentPhase::Approved | api::EnrollmentPhase::Issued => break,
                api::EnrollmentPhase::Cancelled | api::EnrollmentPhase::Expired => {
                    let _ = entry.delete_credential();
                    bail!("Enrollment expired or cancelled")
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            }
        }
        let credential = pending.clone().complete(&client).await?;
        entry.set_password(&serde_json::to_string(&Attempt {
            endpoint: endpoint.clone(),
            pending,
            completed: Some(credential.clone()),
        })?)?;
        let key = self.save_connection(&credential, endpoint)?;
        entry.delete_credential()?;
        Ok(key)
    }
    pub async fn refresh(
        &self,
        key: ConnectionKey,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Snapshot> {
        let client = self.client(key)?;
        let collections = client.collections().await?;
        check(cancelled)?;
        let mut snapshot = Snapshot {
            library: Library {
                id: connection_id(key),
                root: connection_id(key),
                ..Default::default()
            },
            collections,
            lists: vec![],
            artwork: Default::default(),
            album_collections: Default::default(),
            updated: now(),
        };
        for collection in snapshot.collections.iter().filter(|c| c.can_browse) {
            let mut cursor = None;
            let mut seen = BTreeSet::new();
            loop {
                check(cancelled)?;
                let page = client
                    .albums(&ids::BrowseQuery {
                        collection_id: Some(collection.id),
                        cursor,
                        limit: Some(100),
                        ..Default::default()
                    })
                    .await?;
                for album in page.items {
                    check(cancelled)?;
                    let playback = client.music_tracks(album.id).await?;
                    let mut tracks = vec![];
                    let mut coordinates = BTreeSet::new();
                    for row in playback.iter().filter(|t| {
                        t.primary_playback()
                            && t.album_selection == api::MusicAlbumSelection::Included
                    }) {
                        if coordinates.insert((
                            row.capture_id,
                            row.source_representation_id,
                            row.track_position,
                        )) {
                            tracks.push(remote_track(
                                key,
                                album.id,
                                collection.id,
                                &album.artist,
                                row,
                            ));
                        }
                    }
                    tracks.sort_by_key(|t| (t.disc, t.number));
                    let item = album.id.to_string();
                    snapshot
                        .album_collections
                        .insert(item.clone(), collection.id);
                    if let Some(asset) = album.artwork {
                        check(cancelled)?;
                        if let Ok(bytes) = client.thumbnail(collection.id, asset).await {
                            let filename = hex::encode(Sha256::digest(
                                format!("{}/{}/{asset}", connection_id(key), collection.id)
                                    .as_bytes(),
                            ));
                            let path = self.root.join("art").join(filename);
                            self.prune_artwork(bytes.len() as u64)?;
                            std::fs::write(&path, bytes)?;
                            snapshot
                                .artwork
                                .insert(item.clone(), path.to_string_lossy().into_owned());
                        }
                    }
                    snapshot.library.albums.push(Album {
                        id: ItemId {
                            library: connection_id(key),
                            provider: "collector".into(),
                            item,
                        },
                        title: album.title,
                        artist: album.artist,
                        tracks,
                        source_label: collection.name.clone(),
                    });
                }
                cursor = page.next_cursor;
                match &cursor {
                    None => break,
                    Some(c) => {
                        if !seen.insert(c.clone()) || seen.len() > 10000 {
                            bail!("Invalid album pagination")
                        }
                    }
                }
            }
            let mut cursor = None;
            let mut seen = BTreeSet::new();
            loop {
                check(cancelled)?;
                let page = client
                    .lists(
                        collection.id,
                        &api::CollectionListsQuery {
                            search: None,
                            cursor,
                            limit: Some(100),
                        },
                    )
                    .await?;
                for list in page.items {
                    check(cancelled)?;
                    if list.kind == ids::CollectionListKind::MusicTracks {
                        snapshot
                            .lists
                            .push(client.list_snapshot(collection.id, list.id).await?)
                    }
                }
                cursor = page.next_cursor.map(|c| c.parse()).transpose()?;
                match cursor {
                    None => break,
                    Some(c) => {
                        if !seen.insert(c) || seen.len() > 10000 {
                            bail!("Invalid list pagination")
                        }
                    }
                }
            }
        }
        check(cancelled)?;
        self.put("snapshot", &connection_id(key), &snapshot)?;
        Ok(snapshot)
    }
    pub fn list_tracks(&self, key: ConnectionKey, list: &api::CollectionListView) -> Vec<Track> {
        list.entries
            .items
            .iter()
            .filter_map(|entry| {
                let item = entry.item_id?;
                if let Some(audio) = &entry.audio {
                    Some(remote_track(key, item, list.list.collection_id, "", audio))
                } else if let Some(ids::ListTarget::MusicTrack {
                    capture_id,
                    source_representation_id,
                    track_position,
                    ..
                }) = &entry.source
                {
                    self.downloads()
                        .ok()?
                        .into_iter()
                        .find(|d| {
                            d.status == "complete"
                                && d.remote.key == key
                                && d.remote.item == item
                                && d.remote.collection == list.list.collection_id
                                && d.remote.capture == *capture_id
                                && d.remote.source == *source_representation_id
                                && d.remote.position == *track_position
                        })
                        .map(|mut d| {
                            d.track.availability = amp_core::Availability::Downloaded;
                            d.track
                        })
                } else {
                    None
                }
            })
            .collect()
    }
    pub async fn submit(&self, command: ListCommand) -> Result<api::CollectionListSummary> {
        self.put("command", &command.receipt, &command)?;
        let client = self.client(command.key)?;
        let result = if let Some(create) = &command.create {
            client
                .create_list(command.collection, create, &command.receipt)
                .await?
        } else {
            client
                .update_list(
                    command.collection,
                    command.list.context("Missing list")?,
                    command.update.as_ref().context("Missing change")?,
                    &command.receipt,
                )
                .await?
        };
        self.db.lock().unwrap().execute(
            "DELETE FROM records WHERE kind='command' AND id=?",
            [command.receipt],
        )?;
        Ok(result)
    }
    pub fn discard_commands(&self, key: ConnectionKey) -> Result<()> {
        for c in self.pending_commands(key)? {
            self.db.lock().unwrap().execute(
                "DELETE FROM records WHERE kind='command' AND id=?",
                [c.receipt],
            )?;
        }
        Ok(())
    }
    pub fn pending_commands(&self, key: ConnectionKey) -> Result<Vec<ListCommand>> {
        Ok(self
            .all::<ListCommand>("command")?
            .into_iter()
            .filter(|c| c.key == key)
            .collect())
    }
    fn retained_download(&self, remote: &RemoteTrack) -> Result<Option<Download>> {
        Ok(self.downloads()?.into_iter().find(|d| {
            d.status == "complete"
                && d.remote.key == remote.key
                && d.remote.collection == remote.collection
                && d.remote.capture == remote.capture
                && d.remote.source == remote.source
                && d.remote.position == remote.position
        }))
    }
    pub fn downloaded(&self, track: &Track) -> bool {
        parse_remote(track)
            .ok()
            .and_then(|r| self.retained_download(&r).ok().flatten())
            .is_some()
    }
    pub fn can_play(&self, track: &Track) -> bool {
        if self.downloaded(track) {
            return true;
        }
        let Ok(remote) = parse_remote(track) else {
            return false;
        };
        self.client(remote.key).is_ok()
            && remote.audio.as_ref().is_some_and(|a| {
                matches!(a.state, api::MusicTrackAvailabilityState::Available { .. })
            })
    }
    pub fn downloads(&self) -> Result<Vec<Download>> {
        self.all("download")
    }
    pub fn budget(&self) -> Result<u64> {
        Ok(self
            .get("setting", "budget")?
            .unwrap_or(10 * 1024 * 1024 * 1024))
    }
    pub fn set_budget(&self, bytes: u64) -> Result<()> {
        self.put("setting", "budget", &bytes)
    }
    pub fn cancel_downloads(&self) {
        self.downloads_cancel.store(true, Ordering::Release);
        if let Some(token) = self.download_token.lock().unwrap().as_ref() {
            token.cancel()
        }
    }
    pub fn reset_download_cancel(&self) {
        self.downloads_cancel.store(false, Ordering::Release)
    }
    pub fn schedule(&self, tracks: &[Track]) -> Result<()> {
        let mut planned = Vec::new();
        for track in tracks {
            let remote = parse_remote(track)?;
            let Some(audio) = remote.audio.as_ref() else {
                continue;
            };
            let api::MusicTrackAvailabilityState::Available { length, digest, .. } = &audio.state
            else {
                continue;
            };
            if digest.algorithm != ids::HashAlgorithm::Sha256 {
                bail!("Unsupported download digest")
            }
            let id = download_id(&remote)?;
            planned.push(Download {
                id,
                track: track.clone(),
                remote: remote.clone(),
                bytes: length.0,
                status: "pending".into(),
                error: None,
                digest: digest.hex.clone(),
            });
        }
        if planned.is_empty() {
            bail!("No prepared tracks to download")
        }
        let mut db = self.db.lock().unwrap();
        let transaction = db.transaction()?;
        use rusqlite::OptionalExtension;
        let mut existing = std::collections::BTreeMap::new();
        {
            let mut stmt =
                transaction.prepare("SELECT value FROM records WHERE kind='download'")?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let d: Download = serde_json::from_str(&row?)?;
                existing.insert(d.id.clone(), d);
            }
        }
        for record in planned {
            if existing
                .get(&record.id)
                .is_none_or(|d| d.status != "complete")
            {
                existing.insert(record.id.clone(), record);
            }
        }
        let total = existing.values().try_fold(0u64, |n, d| {
            n.checked_add(d.bytes).context("Download size overflow")
        })?;
        let budget: Option<String> = transaction
            .query_row(
                "SELECT value FROM records WHERE kind='setting' AND id='budget'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let budget = budget
            .map(|s| serde_json::from_str::<u64>(&s))
            .transpose()?
            .unwrap_or(10 * 1024 * 1024 * 1024);
        if total > budget {
            bail!("Download budget full; remove downloads or raise the limit")
        }
        for record in existing.values() {
            transaction.execute("INSERT INTO records VALUES ('download',?1,?2) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",params![record.id,serde_json::to_string(record)?])?;
        }
        if fs2::available_space(&self.root)?
            < existing
                .values()
                .filter(|d| d.status != "complete")
                .map(|d| d.bytes)
                .sum::<u64>()
                .saturating_add(256 * 1024 * 1024)
        {
            bail!("Insufficient free disk space; leave at least 256 MiB available")
        }
        transaction.commit()?;
        Ok(())
    }
    pub fn download(&self, track: Track) -> Result<()> {
        if self.downloads_cancel.load(Ordering::Acquire) {
            bail!("Download cancelled")
        };
        let remote = parse_remote(&track)?;
        let audio = remote.audio.as_ref().context("No prepared audio")?;
        let api::MusicTrackAvailabilityState::Available { length, digest, .. } = &audio.state
        else {
            bail!("Audio needs preparation")
        };
        if digest.algorithm != ids::HashAlgorithm::Sha256 {
            bail!("Unsupported download digest")
        };
        let id = download_id(&remote)?;
        let mut record = Download {
            id: id.clone(),
            track,
            remote: remote.clone(),
            bytes: length.0,
            status: "pending".into(),
            error: None,
            digest: digest.hex.clone(),
        };
        // Serialize reservation with every other admission; explicit downloads are never evicted.
        {
            let db = self.db.lock().unwrap();
            use rusqlite::OptionalExtension;
            let old: Option<String> = db
                .query_row(
                    "SELECT value FROM records WHERE kind='download' AND id=?",
                    [&id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(old) = old {
                let old: Download = serde_json::from_str(&old)?;
                if old.status == "complete" {
                    return Ok(());
                }
            }
            let mut stmt =
                db.prepare("SELECT value FROM records WHERE kind='download' AND id<>?")?;
            let mut total = record.bytes;
            for value in stmt.query_map([&id], |r| r.get::<_, String>(0))? {
                let d: Download = serde_json::from_str(&value?)?;
                total = total
                    .checked_add(d.bytes)
                    .context("Download size overflow")?;
            }
            let budget: Option<String> = db
                .query_row(
                    "SELECT value FROM records WHERE kind='setting' AND id='budget'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let budget = budget
                .map(|s| serde_json::from_str::<u64>(&s))
                .transpose()?
                .unwrap_or(10 * 1024 * 1024 * 1024);
            if total > budget {
                bail!("Download budget full; remove downloads or raise the limit")
            }
            db.execute("INSERT INTO records VALUES ('download',?1,?2) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",params![id,serde_json::to_string(&record)?])?;
        }
        let temp = self.root.join("audio").join(format!("{id}.part"));
        let result = (|| -> Result<()> {
            let client = self.client(remote.key)?;
            let api::MusicTrackAvailabilityState::Available {
                representation_id,
                file_index,
                ..
            } = &audio.state
            else {
                unreachable!()
            };
            let cancel = MediaCancellation::default();
            *self.download_token.lock().unwrap() = Some(cancel.clone());
            if self.downloads_cancel.load(Ordering::Acquire) {
                cancel.cancel();
                bail!("Download cancelled")
            };
            let mut reader = client.media_reader(
                remote.item,
                *representation_id,
                *file_index,
                cancel.clone(),
            )?;
            if reader.length() != record.bytes {
                bail!("Media length changed")
            }
            let mut file = std::fs::File::create(&temp)?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            let mut read = 0u64;
            loop {
                if self.downloads_cancel.load(Ordering::Acquire) {
                    cancel.cancel();
                    bail!("Download cancelled")
                }
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                read += n as u64;
                if read > record.bytes {
                    bail!("Oversized download")
                }
                hash.update(&buffer[..n]);
                file.write_all(&buffer[..n])?;
            }
            if read != record.bytes || hex::encode(hash.finalize()) != record.digest {
                bail!("Download integrity check failed")
            }
            if self.downloads_cancel.load(Ordering::Acquire) {
                bail!("Download cancelled")
            }
            file.sync_all()?;
            std::fs::rename(&temp, self.root.join("audio").join(&id))?;
            Ok(())
        })();
        *self.download_token.lock().unwrap() = None;
        match result {
            Ok(()) => {
                record.status = "complete".into();
                record.error = None
            }
            Err(e) => {
                record.status = "failed".into();
                record.error = Some(e.to_string());
                let _ = std::fs::remove_file(temp);
                self.put("download", &id, &record)?;
                return Err(e);
            }
        }
        self.put("download", &id, &record)
    }
    pub fn remove_download(&self, track: &Track) -> Result<()> {
        let remote = parse_remote(track)?;
        let id = self
            .retained_download(&remote)?
            .map(|d| d.id)
            .map(Ok)
            .unwrap_or_else(|| download_id(&remote))?;
        for path in [
            self.root.join("audio").join(&id),
            self.root.join("audio").join(format!("{id}.part")),
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM records WHERE kind='download' AND id=?", [id])?;
        Ok(())
    }
}
fn remote_track(
    key: ConnectionKey,
    item: ids::ItemId,
    collection: ids::CollectionId,
    artist: &str,
    row: &api::MusicTrackAvailability,
) -> Track {
    let remote = RemoteTrack {
        key,
        item,
        collection,
        source: row.source_representation_id,
        capture: row.capture_id,
        position: row.track_position,
        audio: Some(row.clone()),
    };
    Track {
        availability: match row.state {
            api::MusicTrackAvailabilityState::Available { .. } => amp_core::Availability::Available,
            api::MusicTrackAvailabilityState::Preparing => amp_core::Availability::Preparing,
            _ => amp_core::Availability::Unavailable,
        },
        source: Some(amp_core::SourceCoordinates {
            connection: connection_id(key),
            collection: collection.to_string(),
            item: item.to_string(),
            capture: row.capture_id.to_string(),
            representation: row.source_representation_id.to_string(),
            track: row.track_position,
        }),
        id: ItemId {
            library: connection_id(key),
            provider: "collector".into(),
            item: format!(
                "{item}/{}/{}/{}",
                row.capture_id, row.source_representation_id, row.track_position
            ),
        },
        title: row.title.clone(),
        artist: artist.into(),
        number: row.display_position.unwrap_or(row.track_position).into(),
        disc: row.disc_position.unwrap_or(1).into(),
        duration_seconds: row.frame_count.0 as f64 / f64::from(row.sample_rate()),
        access: MediaAccess::Managed {
            provider: "collector".into(),
            item: serde_json::to_string(&remote).expect("serializable remote track"),
        },
    }
}
fn parse_remote(track: &Track) -> Result<RemoteTrack> {
    match &track.access {
        MediaAccess::Managed { provider, item } if provider == "collector" => {
            Ok(serde_json::from_str(item)?)
        }
        _ => bail!("Not a Collector track"),
    }
}
fn download_id(remote: &RemoteTrack) -> Result<String> {
    let audio = remote.audio.as_ref().context("No audio metadata")?;
    let api::MusicTrackAvailabilityState::Available {
        representation_id,
        file_index,
        digest,
        ..
    } = &audio.state
    else {
        bail!("No prepared audio")
    };
    Ok(hex::encode(Sha256::digest(
        format!(
            "{}/{}/{}/{}/{}/{representation_id}/{file_index}/{}",
            connection_id(remote.key),
            remote.collection,
            remote.item,
            remote.capture,
            remote.source,
            digest.hex
        )
        .as_bytes(),
    )))
}
struct CachedReader {
    file: std::fs::File,
    cancellation: MediaCancellation,
}
impl Read for CachedReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Playback cancelled",
            ));
        };
        self.file.read(bytes)
    }
}
impl std::io::Seek for CachedReader {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Playback cancelled",
            ));
        };
        self.file.seek(position)
    }
}
impl Drop for CachedReader {
    fn drop(&mut self) {
        self.cancellation.cancel()
    }
}
impl amp_audio::MediaResolver for Adapter {
    fn resolve(&self, track: &Track) -> Result<amp_audio::ByteSource> {
        let remote = parse_remote(track)?;
        let cancellation = MediaCancellation::default();
        self.media
            .lock()
            .unwrap()
            .retain(|(_, c)| !c.is_cancelled());
        self.media
            .lock()
            .unwrap()
            .push((remote.key, cancellation.clone()));
        if let Some(record) = self.retained_download(&remote)? {
            let mut file = std::fs::File::open(self.root.join("audio").join(&record.id))?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            let mut length = 0;
            loop {
                if cancellation.is_cancelled() {
                    bail!("Playback cancelled")
                };
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
                length += n as u64;
            }
            if length != record.bytes || hex::encode(hash.finalize()) != record.digest {
                bail!("Downloaded audio is corrupt; remove and download again")
            }
            use std::io::Seek;
            file.rewind()?;
            return Ok(amp_audio::ByteSource::new(
                CachedReader { file, cancellation },
                record
                    .remote
                    .audio
                    .as_ref()
                    .map(|audio| media_hint(Some(length), &audio.state))
                    .unwrap_or_else(|| amp_audio::MediaHint::new(Some(length), None, None)),
            ));
        }
        // The shared client prefers pinned private media, then advertised direct
        // HTTPS with scoped grants. It owns retries and public fallback policy;
        // the enrolled account and offline-cache identity stay unchanged.
        let client = self.client(remote.key)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let rows=rt.block_on(async {
            let request=client.music_tracks(remote.item);tokio::pin!(request);
            loop {
                if cancellation.is_cancelled() {return Err(ids::Error::JobCancelled)}
                tokio::select! {result=&mut request=>return result,_=tokio::time::sleep(std::time::Duration::from_millis(20))=>{}}
            }
        })?;
        let audio = rows
            .iter()
            .find(|a| {
                a.capture_id == remote.capture
                    && a.source_representation_id == remote.source
                    && a.track_position == remote.position
                    && a.primary_playback()
            })
            .context("Source no longer available")?;
        let api::MusicTrackAvailabilityState::Available {
            representation_id,
            file_index,
            length,
            ..
        } = &audio.state
        else {
            bail!("Track is preparing or unavailable")
        };
        let reader =
            client.media_reader(remote.item, *representation_id, *file_index, cancellation)?;
        Ok(amp_audio::ByteSource::new(
            reader,
            media_hint(Some(length.0), &audio.state),
        ))
    }
    fn cancel(&self) {
        for (_, token) in self.media.lock().unwrap().drain(..) {
            token.cancel()
        }
    }
}

fn media_hint(
    length: Option<u64>,
    state: &api::MusicTrackAvailabilityState,
) -> amp_audio::MediaHint {
    let media_type = match state {
        api::MusicTrackAvailabilityState::Available { media_type, .. } => Some(media_type.as_str()),
        _ => None,
    };
    let extension = match media_type {
        Some("audio/flac") => Some("flac"),
        Some("audio/mpeg") => Some("mp3"),
        _ => None,
    };
    amp_audio::MediaHint::new(length, extension, media_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct MemorySecrets(Mutex<Vec<EnrolledCredential>>);
    impl NativeCredentialStorage for MemorySecrets {
        fn save(&self, c: &EnrolledCredential) -> ids::Result<()> {
            self.0.lock().unwrap().push(c.clone());
            Ok(())
        }
        fn load(&self, key: ConnectionKey) -> ids::Result<Option<EnrolledCredential>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|c| c.server_id == key.server_id && c.user_id == Some(key.user_id))
                .cloned())
        }
        fn remove(&self, key: ConnectionKey) -> ids::Result<()> {
            self.0
                .lock()
                .unwrap()
                .retain(|c| c.server_id != key.server_id || c.user_id != Some(key.user_id));
            Ok(())
        }
        fn load_legacy(&self, _: ids::ServerId) -> ids::Result<Option<EnrolledCredential>> {
            Ok(None)
        }
        fn remove_legacy(&self, _: ids::ServerId) -> ids::Result<()> {
            Ok(())
        }
    }
    fn key() -> ConnectionKey {
        ConnectionKey {
            server_id: ids::ServerId::new(),
            user_id: ids::UserId::new(),
        }
    }
    #[test]
    fn cleared_selection_can_be_read_and_restored_after_reopening() {
        let root = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let adapter = Adapter::with_credentials(root.path(), secrets.clone()).unwrap();
        let key = key();
        assert_eq!(adapter.selected().unwrap(), None);
        adapter.select(Some(key)).unwrap();
        assert_eq!(adapter.selected().unwrap(), Some(key));
        adapter.select(None).unwrap();
        assert_eq!(adapter.selected().unwrap(), None);
        drop(adapter);
        let adapter = Adapter::with_credentials(root.path(), secrets).unwrap();
        assert_eq!(adapter.selected().unwrap(), None);
        adapter.select(Some(key)).unwrap();
        assert_eq!(adapter.selected().unwrap(), Some(key));
    }
    fn track(key: ConnectionKey, data: &[u8]) -> Track {
        let row = api::MusicTrackAvailability {
            album_selection: api::MusicAlbumSelection::Included,
            display_position: None,
            audio_format: None,
            capture_id: ids::CaptureId::new(),
            source_label: "fixture".into(),
            disc_position: Some(1),
            source_representation_id: ids::RepresentationId::new(),
            track_position: 1,
            start_frame: ids::PcmFrame(0),
            frame_count: ids::PcmFrame(588),
            title: "Fixture".into(),
            profile: Some(ids::ConversionProfile::Flac),
            state: api::MusicTrackAvailabilityState::Available {
                representation_id: ids::RepresentationId::new(),
                file_id: ids::PackageFileId::new(),
                file_index: 0,
                length: ids::ByteLength(data.len() as u64),
                media_type: "audio/flac".into(),
                digest: ids::Digest::sha256(data),
            },
            can_prepare: false,
        };
        remote_track(
            key,
            ids::ItemId::new(),
            ids::CollectionId::new(),
            "Artist",
            &row,
        )
    }
    #[test]
    fn snapshots_isolate_servers_accounts_and_survive_restart() {
        let root = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let a = Adapter::with_credentials(root.path(), secrets.clone()).unwrap();
        let first = key();
        let second = ConnectionKey {
            server_id: first.server_id,
            user_id: ids::UserId::new(),
        };
        let third = ConnectionKey {
            server_id: ids::ServerId::new(),
            user_id: first.user_id,
        };
        for (key, name) in [(first, "one"), (second, "two"), (third, "three")] {
            let snapshot = Snapshot {
                library: Library {
                    id: connection_id(key),
                    root: name.into(),
                    ..Default::default()
                },
                collections: vec![],
                lists: vec![],
                artwork: Default::default(),
                album_collections: Default::default(),
                updated: 7,
            };
            a.put("snapshot", &connection_id(key), &snapshot).unwrap();
        }
        a.select(Some(second)).unwrap();
        drop(a);
        let a = Adapter::with_credentials(root.path(), secrets).unwrap();
        assert_eq!(a.selected().unwrap(), Some(second));
        assert_eq!(a.snapshot(first).unwrap().unwrap().library.root, "one");
        assert_eq!(a.snapshot(second).unwrap().unwrap().library.root, "two");
        assert_eq!(a.snapshot(third).unwrap().unwrap().library.root, "three");
    }
    #[test]
    fn download_scope_reservation_is_atomic_durable_and_never_evicts() {
        let root = tempfile::tempdir().unwrap();
        let a = Adapter::with_credentials(root.path(), Arc::new(MemorySecrets::default())).unwrap();
        let t = track(key(), b"1234");
        let other = track(key(), b"12345");
        a.set_budget(8).unwrap();
        assert!(a.schedule(&[t.clone(), other.clone()]).is_err());
        assert!(a.downloads().unwrap().is_empty());
        a.schedule(std::slice::from_ref(&t)).unwrap();
        assert_eq!(a.downloads().unwrap()[0].status, "pending");
        assert!(a.schedule(&[other]).is_err());
        assert_eq!(a.downloads().unwrap().len(), 1);
        a.remove_download(&t).unwrap();
        assert!(a.downloads().unwrap().is_empty());
    }
    #[test]
    fn completed_audio_is_retained_offline_and_corruption_fails_closed() {
        use amp_audio::MediaResolver;
        let root = tempfile::tempdir().unwrap();
        let a = Adapter::with_credentials(root.path(), Arc::new(MemorySecrets::default())).unwrap();
        let data = b"downloaded fixture bytes";
        let t = track(key(), data);
        a.schedule(std::slice::from_ref(&t)).unwrap();
        let mut d = a.downloads().unwrap().pop().unwrap();
        std::fs::write(a.root.join("audio").join(&d.id), data).unwrap();
        d.status = "complete".into();
        a.put("download", &d.id, &d).unwrap();
        let mut reader = a.resolve(&t).unwrap();
        let mut actual = vec![];
        reader.reader_mut().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, data);
        std::fs::write(a.root.join("audio").join(&d.id), b"corrupt").unwrap();
        assert!(a.resolve(&t).is_err());
        a.remove_download(&t).unwrap();
        assert!(!a.can_play(&t) || a.client(d.remote.key).is_err());
    }
    #[test]
    fn unresolved_list_command_preserves_exact_receipt_and_input() {
        let root = tempfile::tempdir().unwrap();
        let a = Adapter::with_credentials(root.path(), Arc::new(MemorySecrets::default())).unwrap();
        let key = key();
        let command = ListCommand {
            key,
            collection: ids::CollectionId::new(),
            list: None,
            create: Some(api::CreateCollectionList {
                kind: None,
                name: "Original".into(),
            }),
            update: None,
            receipt: "receipt".into(),
        };
        a.put("command", "receipt", &command).unwrap();
        drop(a);
        let a = Adapter::with_credentials(root.path(), Arc::new(MemorySecrets::default())).unwrap();
        let saved = a.pending_commands(key).unwrap().pop().unwrap();
        assert_eq!(saved.receipt, "receipt");
        assert_eq!(saved.create.unwrap().name, "Original");
        a.discard_commands(key).unwrap();
        assert!(a.pending_commands(key).unwrap().is_empty());
    }
}
