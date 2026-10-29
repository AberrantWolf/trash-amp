//! Catalog jobs, accepted completion and persistence. No UI event loop is needed.
use crate::sources::{AccessStatus, RefreshKind, SourceChange, SourceDescriptor, SourceProvider};
use crate::{Notice, Playback};
use amp_core::{Album, Library, Preferences};
use amp_library::{FilesystemProvider, SavedFolder, ScanProgress, Store};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Local,
    Transient,
    External(String),
}
#[derive(Clone)]
pub struct Snapshot {
    pub generation: u64,
    pub library_revision: u64,
    pub replacement_revision: u64,
    pub library: Arc<Library>,
    pub artwork: Arc<BTreeMap<String, String>>,
    pub folders: Arc<[SavedFolder]>,
    pub preferences: Preferences,
    pub source: Source,
    pub sources: Arc<[SourceDescriptor]>,
    pub requested_source: Option<String>,
    pub source_access: Option<AccessStatus>,
    pub source_online: Option<bool>,
    /// Completed source jobs, including failures; independent of catalog publication.
    pub source_load_revision: u64,
    pub source_message: Option<String>,
    pub scanning: bool,
    pub enriching: bool,
    pub progress: Option<ScanProgress>,
    pub notice: Option<Notice>,
}
/// A provider's catalog is not assumed to be a filesystem hierarchy.
#[derive(Clone)]
pub struct ExternalCatalog {
    /// Runtime connectivity, when meaningful for this source. Never inferred from songs.
    pub online: Option<bool>,
    pub source: String,
    pub library: Library,
    pub artwork: BTreeMap<String, String>,
}
/// Compensation runs on the catalog owner if cancellation wins during persistence.
pub type SelectionUndo = Box<dyn FnOnce() -> Result<()> + Send>;
pub type SelectionCommit = Arc<dyn Fn() -> Result<SelectionUndo> + Send + Sync>;

/// Shared cancellation scope for local and external source operations. Advancing
/// it cancels jobs immediately. In-flight selection writes are compensated before
/// rejection; final acceptance and advancement are ordered without holding up
/// callers for persistence.
#[derive(Clone)]
pub struct Generation {
    value: Arc<AtomicU64>,
    acceptance: Arc<Mutex<()>>,
    tx: mpsc::Sender<Command>,
}
impl Generation {
    pub fn current(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }
    pub fn advance(&self) -> u64 {
        let _acceptance = self.acceptance.lock().unwrap();
        let version = self.value.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.tx.send(Command::Invalidate(version));
        version
    }
}
#[derive(Clone)]
pub struct Catalog {
    tx: mpsc::Sender<Command>,
    snapshot: Arc<RwLock<Snapshot>>,
    generation: Generation,
    network: Arc<AtomicBool>,
}
impl Catalog {
    /// Launcher bootstrap only; do not wait on catalog work from a presentation callback.
    pub fn bootstrap_snapshot(&self) -> Result<Snapshot> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::Read(tx));
        rx.recv()
            .map_err(|_| anyhow::anyhow!("Catalog worker stopped"))
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().unwrap().clone()
    }
    pub fn generation(&self) -> Generation {
        self.generation.clone()
    }
    pub fn scan_folder(&self, root: PathBuf) {
        let v = self.generation.advance();
        self.send(Command::Scan(v, root));
    }
    pub fn open_saved_folder(&self, id: String) {
        let v = self.generation.advance();
        self.send(Command::Saved(v, id));
    }
    pub fn restore_local(&self) {
        let v = self.generation.advance();
        self.send(Command::Restore(v));
    }
    pub fn show_transient(&self, library: Library) {
        let v = self.generation.advance();
        self.send(Command::Transient(v, library));
    }
    pub fn enrich_current(&self) {
        self.send(Command::Enrich(self.generation.current()));
    }
    pub fn save_preferences(&self, preferences: Preferences) {
        self.network
            .store(preferences.automatic_metadata, Ordering::Release);
        self.send(Command::Preferences(preferences));
    }
    /// Run only on accepted local selection, never on the rendering thread.
    /// Serialize provider settings/receipt persistence alongside catalog commits.
    pub fn persist(&self, operation: Box<dyn FnOnce() -> Result<()> + Send>) {
        self.send(Command::Persist(operation));
    }
    pub fn on_local_selection(&self, commit: SelectionCommit) {
        self.send(Command::LocalCommit(commit));
    }
    /// Provider workers publish completed snapshots here, before notifying any UI.
    /// The selection commit runs on the owner only if this generation is current.
    pub fn complete_external(
        &self,
        version: u64,
        data: ExternalCatalog,
        commit: SelectionCommit,
    ) -> mpsc::Receiver<Result<bool>> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::External(version, data, commit, tx));
        rx
    }
    /// Register host capabilities once before presentation starts.
    pub fn register_sources(&self, providers: Vec<Arc<dyn SourceProvider>>, folders: bool) {
        self.send(Command::RegisterSources(providers, folders));
    }
    pub fn source_changed(&self, change: SourceChange) {
        self.send(Command::SourceChanged(change));
    }
    pub fn refresh_sources(&self) {
        self.send(Command::RefreshSources);
    }
    pub fn select_source(&self, id: String) {
        let v = self.generation.advance();
        if id == "folders" {
            self.send(Command::Restore(v));
        } else {
            self.send(Command::OpenSource(v, id, true, RefreshKind::Live));
        }
    }
    pub fn open_source_at(&self, version: u64, id: String, interactive: bool) {
        self.send(Command::OpenSource(
            version,
            id,
            interactive,
            RefreshKind::Live,
        ));
    }
    pub fn refresh_source(&self) {
        let snapshot = self.snapshot();
        match snapshot.source {
            Source::External(id) => self.select_source(id),
            Source::Local if !snapshot.library.root.is_empty() => {
                self.scan_folder(snapshot.library.root.clone().into())
            }
            _ => {}
        }
    }
    /// Lifecycle/notification refresh must never cause an authorization prompt.
    pub fn revalidate_source(&self) {
        if let Source::External(id) = self.snapshot().source {
            self.source_changed(SourceChange {
                id,
                refresh: RefreshKind::Live,
            });
        }
    }
    fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

/// Injectable local jobs keep race tests independent of the network and mounts.
pub trait Backend: Send + Sync {
    fn scan(
        &self,
        root: PathBuf,
        preferences: Preferences,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(ScanProgress),
    ) -> Result<Library>;
    fn enrich(
        &self,
        library: &mut Library,
        store: &Store,
        network: &dyn Fn() -> bool,
        cancelled: &dyn Fn() -> bool,
        update: &mut dyn FnMut(&Album),
    ) -> Result<()>;
}
struct LocalBackend;
impl Backend for LocalBackend {
    fn scan(
        &self,
        root: PathBuf,
        preferences: Preferences,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(ScanProgress),
    ) -> Result<Library> {
        FilesystemProvider { root, preferences }.scan_with_progress(cancelled, progress)
    }
    fn enrich(
        &self,
        library: &mut Library,
        store: &Store,
        network: &dyn Fn() -> bool,
        cancelled: &dyn Fn() -> bool,
        update: &mut dyn FnMut(&Album),
    ) -> Result<()> {
        amp_library::metadata::enrich(library, store, network, cancelled, update)
    }
}
pub struct CatalogService {
    catalog: Catalog,
    worker: Option<thread::JoinHandle<()>>,
}
impl CatalogService {
    pub fn open(path: &Path, playback: Playback) -> Result<Self> {
        Self::with_backend(path, playback, Arc::new(LocalBackend))
    }
    pub fn with_backend(
        path: &Path,
        playback: Playback,
        backend: Arc<dyn Backend>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Bootstrap runs in the launcher, before presentation starts. The live
        // connection and all subsequent reads/writes move to the catalog worker.
        let store = Store::open(path)?;
        let preferences = store.preferences()?;
        let state = Snapshot {
            generation: 0,
            library_revision: 0,
            replacement_revision: 0,
            library: Arc::new(store.library()?.unwrap_or_default()),
            artwork: Arc::default(),
            folders: store.saved_folders()?.into(),
            preferences: preferences.clone(),
            source: Source::Local,
            sources: Arc::from([]),
            requested_source: None,
            source_access: None,
            source_online: None,
            source_load_revision: 0,
            source_message: None,
            scanning: false,
            enriching: false,
            progress: None,
            notice: None,
        };
        let snapshot = Arc::new(RwLock::new(state.clone()));
        let (tx, rx) = mpsc::channel();
        let generation = Generation {
            value: Arc::new(AtomicU64::new(0)),
            acceptance: Arc::default(),
            tx: tx.clone(),
        };
        let network = Arc::new(AtomicBool::new(preferences.automatic_metadata));
        let catalog = Catalog {
            tx,
            snapshot: snapshot.clone(),
            generation: generation.clone(),
            network: network.clone(),
        };
        let cache = path.to_owned();
        let worker = thread::Builder::new()
            .name("amp-catalog".into())
            .spawn(move || {
                Worker {
                    store,
                    cache,
                    backend,
                    playback,
                    state,
                    published: snapshot,
                    generation,
                    network,
                    local_commit: None,
                    providers: Vec::new(),
                    folders_enabled: true,
                    source_job: None,
                    source_registry: BTreeMap::new(),
                    next_source_request: 0,
                    notice_revision: 0,
                    metadata_dirty: false,
                }
                .run(rx);
            })?;
        Ok(Self {
            catalog,
            worker: Some(worker),
        })
    }
    pub fn catalog(&self) -> Catalog {
        self.catalog.clone()
    }
}
impl Drop for CatalogService {
    fn drop(&mut self) {
        self.catalog.generation.advance();
        self.catalog.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum Command {
    RegisterSources(Vec<Arc<dyn SourceProvider>>, bool),
    SourceChanged(SourceChange),
    RefreshSources,
    SourceArtwork(SourceRequest, BTreeMap<String, String>),
    OpenSource(u64, String, bool, RefreshKind),
    SourceLoaded(SourceRequest, bool, Result<ExternalCatalog>),
    Invalidate(u64),
    Scan(u64, PathBuf),
    Saved(u64, String),
    Restore(u64),
    Transient(u64, Library),
    Enrich(u64),
    Preferences(Preferences),
    LocalCommit(SelectionCommit),
    Persist(Box<dyn FnOnce() -> Result<()> + Send>),
    External(
        u64,
        ExternalCatalog,
        SelectionCommit,
        mpsc::Sender<Result<bool>>,
    ),
    Progress(u64, ScanProgress),
    Scanned(u64, Result<Library>),
    Metadata(u64, Album),
    Enriched(u64, Result<()>),
    Shutdown,
    Read(mpsc::Sender<Snapshot>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceRequest {
    generation: u64,
    serial: u64,
    id: String,
}
struct SourceJob {
    request: SourceRequest,
    cancelled: Arc<AtomicBool>,
    loading: bool,
    dirty: Option<RefreshKind>,
}
impl Drop for SourceJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
struct Worker {
    store: Store,
    cache: PathBuf,
    backend: Arc<dyn Backend>,
    playback: Playback,
    state: Snapshot,
    published: Arc<RwLock<Snapshot>>,
    generation: Generation,
    network: Arc<AtomicBool>,
    local_commit: Option<SelectionCommit>,
    providers: Vec<Arc<dyn SourceProvider>>,
    folders_enabled: bool,
    source_job: Option<SourceJob>,
    source_registry: BTreeMap<String, Arc<dyn SourceProvider>>,
    next_source_request: u64,
    notice_revision: u64,
    metadata_dirty: bool,
}
impl Worker {
    fn current(&self, v: u64) -> bool {
        self.generation.current() == v
    }
    fn publish(&self) {
        *self.published.write().unwrap() = self.state.clone();
    }
    fn notice(&mut self, message: String) {
        self.notice_revision += 1;
        self.state.notice = Some(Notice {
            revision: self.notice_revision,
            message,
        });
    }
    fn select_local(
        &mut self,
        v: u64,
        folder: Option<&str>,
        replacement: Option<(Library, Source)>,
    ) -> Result<bool> {
        let previous = self.store.library()?.map(|library| library.id);
        let previous_source = self.store.active_source()?;
        let undo: SelectionUndo = match &self.local_commit {
            Some(commit) => commit()?,
            None => Box::new(|| Ok(())),
        };
        if let Some(folder) = folder
            && let Err(error) = self.store.select_folder(folder)
        {
            undo()?;
            return Err(error);
        }
        if let Err(error) = self.store.save_active_source("folders") {
            let restored = if folder.is_some() {
                self.store.restore_folder_selection(previous.as_deref())
            } else {
                Ok(())
            };
            let provider_restored = undo();
            restored?;
            provider_restored?;
            return Err(error);
        }
        let acceptance = self.generation.acceptance.clone();
        let accepted = acceptance.lock().unwrap();
        if !self.current(v) {
            drop(accepted);
            let restored_folder = if folder.is_some() {
                self.store.restore_folder_selection(previous.as_deref())
            } else {
                Ok(())
            };
            let restored_provider = undo();
            self.store
                .restore_active_source(previous_source.as_deref())?;
            restored_folder?;
            restored_provider?;
            return Ok(false);
        }
        if let Some((library, source)) = replacement {
            self.replace(library, BTreeMap::new(), source);
        }
        Ok(true)
    }
    fn replace(&mut self, library: Library, artwork: BTreeMap<String, String>, source: Source) {
        let same_external = matches!(&source, Source::External(_))
            && self.state.source == source
            && self.state.library.id == library.id;
        if !same_external {
            self.playback.set_default_album(None);
        }
        self.state.library = Arc::new(library);
        self.state.artwork = Arc::new(artwork);
        self.playback
            .refresh_catalog_metadata(self.state.library.clone(), self.state.artwork.clone());
        self.state.source = source;
        self.state.source_online = None;
        self.state.library_revision += 1;
        if !same_external {
            self.state.replacement_revision += 1;
        }
        self.state.scanning = false;
        self.state.enriching = false;
    }
    fn run(mut self, rx: mpsc::Receiver<Command>) {
        'requests: while let Ok(command) = rx.recv() {
            if matches!(command, Command::Shutdown) {
                break;
            }
            self.apply(command);
            // Cached enrichment can emit many albums together. Publish once
            // per bounded batch so copy-on-write clones a catalog once per batch.
            for _ in 0..63 {
                let Ok(command) = rx.try_recv() else { break };
                if matches!(command, Command::Shutdown) {
                    break 'requests;
                }
                self.apply(command);
            }
            self.publish();
        }
        let _ = self.flush_metadata();
    }
    fn apply(&mut self, command: Command) {
        if let Err(error) = self.command(command) {
            self.notice(error.to_string());
        }
    }
    fn flush_metadata(&mut self) -> Result<()> {
        if self.metadata_dirty {
            self.store.save_library_snapshot(&self.state.library)?;
            self.metadata_dirty = false;
        }
        Ok(())
    }
    fn accept_source(&mut self, v: u64, id: &str, data: ExternalCatalog) -> Result<bool> {
        if !self.current(v) {
            return Ok(false);
        }
        let provider = self.source_provider(id)?;
        if provider.catalog_cache() {
            self.store
                .save_provider_snapshot(id, &data.library, &data.artwork)?;
        }
        let previous = self.store.active_source()?;
        self.store.save_active_source(id)?;
        let acceptance = self.generation.acceptance.clone();
        let accepted = acceptance.lock().unwrap();
        if !self.current(v) {
            drop(accepted);
            self.store.restore_active_source(previous.as_deref())?;
            return Ok(false);
        }
        self.replace(data.library, data.artwork, Source::External(id.into()));
        self.state.source_online = data.online;
        Ok(true)
    }
    fn source_provider(&self, id: &str) -> Result<Arc<dyn SourceProvider>> {
        self.source_registry
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Music source unavailable: {id}"))
    }
    fn source_request_current(&self, request: &SourceRequest) -> bool {
        self.current(request.generation)
            && self
                .source_job
                .as_ref()
                .is_some_and(|job| &job.request == request)
    }
    fn update_sources(&mut self) -> Result<()> {
        let mut entries = Vec::new();
        let mut registry = BTreeMap::new();
        if self.folders_enabled {
            entries.push(SourceDescriptor {
                id: "folders".into(),
                name: "Local folder".into(),
                provider: "file".into(),
            });
        }
        for provider in &self.providers {
            for descriptor in provider.descriptors()? {
                if descriptor.id == "folders"
                    || registry
                        .insert(descriptor.id.clone(), provider.clone())
                        .is_some()
                {
                    anyhow::bail!("Duplicate or reserved music source: {}", descriptor.id);
                }
                entries.push(descriptor);
            }
        }
        self.source_registry = registry;
        self.state.sources = entries.into();
        Ok(())
    }
    fn revoke_access(&mut self, id: &str) -> Result<()> {
        if let Some(descriptor) = self.state.sources.iter().find(|d| d.id == id) {
            self.playback.revoke_provider(descriptor.provider.clone());
        }
        if self.state.source == Source::External(id.into()) {
            self.replace(
                Library::default(),
                BTreeMap::new(),
                Source::External(id.into()),
            );
        }
        // Permission revocation must stop playback and hide metadata even if
        // cache removal encounters a filesystem or database failure.
        let removed = self.store.remove_provider_snapshot(id);
        let purged = self.source_provider(id)?.revoke_cached_data();
        removed?;
        purged?;
        Ok(())
    }
    fn open_source(
        &mut self,
        v: u64,
        id: String,
        interactive: bool,
        refresh: RefreshKind,
    ) -> Result<()> {
        self.update_sources()?;
        let provider = self.source_provider(&id)?;
        if let Some(job) = &mut self.source_job
            && job.request.generation == v
            && job.request.id == id
            && job.loading
        {
            job.dirty = Some(job.dirty.map_or(refresh, |pending| pending.max(refresh)));
            return Ok(());
        }
        let previous_online = if self.state.source == Source::External(id.clone()) {
            self.state.source_online
        } else {
            None
        };
        self.state.requested_source = Some(id.clone());
        let access = provider.access_status();
        self.state.source_access = Some(access);
        self.state.source_message = Some(if access == AccessStatus::NotDetermined {
            "Music library permission is required.".into()
        } else {
            "Loading music library…".into()
        });
        if provider.cache_allowed() {
            if provider.catalog_cache()
                && let Some((library, artwork)) = self.store.provider_snapshot(&id)?
            {
                self.accept_source(
                    v,
                    &id,
                    ExternalCatalog {
                        online: previous_online,
                        source: id.clone(),
                        library,
                        artwork,
                    },
                )?;
            }
        } else {
            self.revoke_access(&id)?;
        }
        self.next_source_request += 1;
        let request = SourceRequest {
            generation: v,
            serial: self.next_source_request,
            id,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.source_job = Some(SourceJob {
            request: request.clone(),
            cancelled: cancelled.clone(),
            loading: true,
            dirty: None,
        });
        self.state.scanning = true;
        let generation = self.generation.clone();
        let online = previous_online;
        let tx = self.generation.tx.clone();
        let spawned = thread::Builder::new()
            .name("amp-source".into())
            .spawn(move || {
                let obsolete = || generation.current() != v || cancelled.load(Ordering::Acquire);
                if refresh == RefreshKind::Cached {
                    let result = provider
                        .project_cached(&request.id, online)
                        .and_then(|data| {
                            data.ok_or_else(|| anyhow::anyhow!("Cached music source unavailable"))
                        });
                    let _ = tx.send(Command::SourceLoaded(request, true, result));
                    return;
                }
                if provider.cache_allowed()
                    && !obsolete()
                    && let Ok(Some(data)) = provider.cached(&request.id)
                {
                    let _ = tx.send(Command::SourceLoaded(request.clone(), false, Ok(data)));
                }
                if obsolete() {
                    return;
                }
                let result = provider.load(&request.id, interactive, &obsolete);
                // Only adapters that actually supply artwork retain another catalog.
                let artwork_catalog = if provider.has_artwork() {
                    result.as_ref().ok().cloned()
                } else {
                    None
                };
                let _ = tx.send(Command::SourceLoaded(request.clone(), true, result));
                if let Some(data) = artwork_catalog
                    && !obsolete()
                    && let Ok(Some(artwork)) = provider.artwork(&data, &obsolete)
                {
                    let _ = tx.send(Command::SourceArtwork(request, artwork));
                }
            });
        if let Err(error) = spawned {
            self.state.scanning = false;
            self.source_job = None;
            self.state.requested_source = None;
            self.state.source_message =
                Some(format!("Could not start music library load: {error}"));
            return Err(error.into());
        }
        Ok(())
    }
    fn queue_source_refresh(&self, id: String, refresh: RefreshKind) {
        // Reloads within the accepted source have their own request ticket.
        // They must not cancel an enrollment/list mutation or change source selection.
        let _ = self.generation.tx.send(Command::OpenSource(
            self.generation.current(),
            id,
            false,
            refresh,
        ));
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::RefreshSources => self.update_sources()?,
            Command::RegisterSources(providers, folders) => {
                let previous_providers = std::mem::replace(&mut self.providers, providers);
                let previous_folders = std::mem::replace(&mut self.folders_enabled, folders);
                if let Err(error) = self.update_sources() {
                    self.providers = previous_providers;
                    self.folders_enabled = previous_folders;
                    return Err(error);
                }
                for provider in &self.providers {
                    let tx = self.generation.tx.clone();
                    provider.observe(Arc::new(move |change| {
                        let _ = tx.send(Command::SourceChanged(change));
                    }));
                }
                let saved = self.store.active_source()?;
                let selected = if let Some(saved) = saved {
                    Some(saved)
                } else {
                    self.providers
                        .iter()
                        .find_map(|p| p.legacy_selection().transpose())
                        .transpose()?
                };
                if let Some(id) = selected
                    .filter(|id| id != "folders" && self.state.sources.iter().any(|s| &s.id == id))
                {
                    self.replace(
                        Library::default(),
                        BTreeMap::new(),
                        Source::External(id.clone()),
                    );
                    self.open_source(self.generation.current(), id, false, RefreshKind::Live)?;
                }
            }
            Command::SourceArtwork(request, artwork)
                if self.source_request_current(&request)
                    && self.state.source == Source::External(request.id.clone())
                    && self.source_provider(&request.id)?.cache_allowed() =>
            {
                self.store
                    .save_provider_snapshot(&request.id, &self.state.library, &artwork)?;
                self.state.artwork = Arc::new(artwork);
                self.state.library_revision += 1;
                self.playback.refresh_catalog_metadata(
                    self.state.library.clone(),
                    self.state.artwork.clone(),
                );
            }
            Command::SourceChanged(change) => {
                let SourceChange { id, refresh } = change;
                // New connections may have been enrolled after registration.
                self.update_sources()?;
                let revoked = !self.source_provider(&id)?.cache_allowed();
                if revoked {
                    if self
                        .source_job
                        .as_ref()
                        .is_some_and(|job| job.request.id == id)
                    {
                        self.source_job = None;
                    }
                    self.revoke_access(&id)?;
                }
                let loading_same = self.source_job.as_ref().is_some_and(|job| {
                    job.loading && job.request.id == id && self.current(job.request.generation)
                });
                if !revoked && loading_same {
                    let job = self.source_job.as_mut().unwrap();
                    job.dirty = Some(job.dirty.map_or(refresh, |pending| pending.max(refresh)));
                } else if self.state.generation == self.generation.current()
                    && self.state.source == Source::External(id.clone())
                    && self
                        .state
                        .requested_source
                        .as_ref()
                        .is_none_or(|pending| pending == &id)
                {
                    self.queue_source_refresh(id, refresh);
                }
            }
            Command::OpenSource(v, id, interactive, refresh) if self.current(v) => {
                self.open_source(v, id, interactive, refresh)?
            }
            Command::SourceLoaded(request, final_result, result)
                if self.source_request_current(&request) =>
            {
                let provider = self.source_provider(&request.id)?;
                let refresh_after = if final_result {
                    self.source_job.as_ref().and_then(|job| job.dirty)
                } else {
                    None
                };
                if final_result {
                    self.state.source_load_revision += 1;
                    self.source_job.as_mut().unwrap().loading = false;
                    self.state.scanning = false;
                    self.state.requested_source = None;
                }
                let completion = (|| -> Result<()> {
                    // Access can be revoked after a job returned data but before it is admitted.
                    if !provider.cache_allowed() {
                        self.revoke_access(&request.id)?;
                        return match result {
                            Err(error) => Err(error),
                            Ok(_) => {
                                Err(anyhow::anyhow!("Music library access denied or restricted"))
                            }
                        };
                    }
                    match result {
                        Ok(data) => {
                            let total: usize =
                                data.library.albums.iter().map(|a| a.tracks.len()).sum();
                            let playable = data
                                .library
                                .albums
                                .iter()
                                .flat_map(|a| &a.tracks)
                                .filter(|t| t.playback_block().is_none())
                                .count();
                            self.state.source_message = Some(if total == 0 {
                                "No music found in this library.".into()
                            } else if playable == 0 {
                                "No playable songs are available in this library.".into()
                            } else {
                                format!(
                                    "{} albums · {playable} playable songs",
                                    data.library.albums.len()
                                )
                            });
                            self.accept_source(request.generation, &request.id, data)?;
                        }
                        Err(error) => return Err(error),
                    }
                    Ok(())
                })();
                self.state.source_access = Some(provider.access_status());
                self.state.scanning = !final_result;
                if let Err(error) = completion {
                    let message = format!("Could not open music source: {error:#}");
                    self.state.source_message = Some(message.clone());
                    self.notice(message);
                }
                if let Some(refresh) = refresh_after {
                    self.queue_source_refresh(request.id, refresh);
                }
            }
            Command::Invalidate(v) if self.current(v) => {
                let flushed = self.flush_metadata();
                self.state.generation = v;
                self.state.scanning = false;
                self.source_job = None;
                self.state.requested_source = None;
                self.state.source_access = None;
                self.state.source_message = None;
                self.state.enriching = false;
                self.state.progress = None;
                self.state.notice = None;
                if let Err(error) = flushed {
                    self.notice(format!("Metadata cache save failed: {error}"));
                }
            }
            Command::Scan(v, root) if self.current(v) => {
                let replacement = if self.state.source != Source::Local {
                    Some((self.store.library()?.unwrap_or_default(), Source::Local))
                } else {
                    None
                };
                if !self.select_local(v, None, replacement)? {
                    return Ok(());
                }
                self.state.scanning = true;
                self.state.progress = None;
                self.notice("Scanning music folder…".into());
                let backend = self.backend.clone();
                let tx = self.generation.tx.clone();
                let generation = self.generation.clone();
                let preferences = self.state.preferences.clone();
                let spawned = thread::Builder::new()
                    .name("amp-scan".into())
                    .spawn(move || {
                        let mut last =
                            std::time::Instant::now() - std::time::Duration::from_secs(1);
                        let mut total = None;
                        let result = backend.scan(
                            root,
                            preferences,
                            &|| generation.current() != v,
                            &mut |n| {
                                if n.total != total
                                    || n.total == Some(n.completed)
                                    || last.elapsed() >= std::time::Duration::from_millis(100)
                                {
                                    let _ = tx.send(Command::Progress(v, n));
                                    total = n.total;
                                    last = std::time::Instant::now();
                                }
                            },
                        );
                        let _ = tx.send(Command::Scanned(v, result));
                    });
                if let Err(error) = spawned {
                    self.state.scanning = false;
                    return Err(error.into());
                }
            }
            Command::Progress(v, progress) if self.current(v) => {
                self.state.progress = Some(progress);
                self.notice(match progress.total {
                    Some(total) => format!("Scanning… {} / {total} files", progress.completed),
                    None => format!("Finding files… {} found", progress.discovered),
                });
            }
            Command::Scanned(v, result) if self.current(v) => {
                self.state.scanning = false;
                match result {
                    Ok(library) => {
                        let saved = self.store.save_library_snapshot(&library);
                        if let Ok(folders) = self.store.saved_folders() {
                            self.state.folders = folders.into();
                        }
                        for warning in &library.warnings {
                            eprintln!("{warning}");
                        }
                        let status = format!(
                            "{} files • {} albums • {} scan warnings • checking metadata…",
                            self.state.progress.and_then(|n| n.total).unwrap_or(0),
                            library.albums.len(),
                            library.warnings.len()
                        );
                        let folder = library.id.clone();
                        let selection =
                            if saved.is_ok() && !library.root.is_empty() && library.id != "demo" {
                                Some(folder.as_str())
                            } else {
                                None
                            };
                        if !self.select_local(v, selection, Some((library, Source::Local)))? {
                            return Ok(());
                        }
                        self.notice(match saved {
                            Ok(()) => status,
                            Err(error) => format!("{status}; cache save failed: {error}"),
                        });
                        self.enrich(v)?;
                    }
                    Err(error) => self.notice(format!("Library unchanged: {error:#}")),
                }
            }
            Command::Saved(v, id) if self.current(v) => {
                let library = self
                    .store
                    .library_by_id(&id)?
                    .ok_or_else(|| anyhow::anyhow!("Saved folder library is unavailable"))?;
                if !self.select_local(v, Some(&id), Some((library, Source::Local)))? {
                    return Ok(());
                }
                self.notice(
                    "Saved folder library opened. Refresh folder to scan for changes.".into(),
                );
            }
            Command::Restore(v) if self.current(v) => {
                let library = self.store.library()?.unwrap_or_default();
                self.select_local(v, None, Some((library, Source::Local)))?;
            }
            Command::Transient(v, library) if self.current(v) => {
                if !self.select_local(v, None, Some((library, Source::Transient)))? {
                    return Ok(());
                }
                self.notice(
                    "2,000 generated albums. This fixture contains no playable audio.".into(),
                );
            }
            Command::Enrich(v) if self.current(v) && self.state.source == Source::Local => {
                self.enrich(v)?;
            }
            Command::Metadata(v, album)
                if self.current(v) && self.state.source == Source::Local =>
            {
                let library = Arc::make_mut(&mut self.state.library);
                if let Some(index) = library.albums.iter().position(|a| a.id == album.id) {
                    library.albums[index] = album.clone();
                    self.state.library_revision += 1;
                    self.metadata_dirty = true;
                    self.playback.update_metadata(album);
                }
            }
            Command::Enriched(v, result)
                if self.current(v) && self.state.source == Source::Local =>
            {
                self.state.enriching = false;
                // Enrichment may refresh a catalog, but must never reselect a folder.
                if let Err(error) = self.flush_metadata() {
                    self.notice(format!("Metadata cache save failed: {error}"));
                } else if let Err(error) = result
                    && self.network.load(Ordering::Acquire)
                {
                    self.notice(format!(
                        "Online metadata paused: {error}; local library available"
                    ));
                } else {
                    let library = &self.state.library;
                    let unmatched = library
                        .albums
                        .iter()
                        .filter(|a| a.source_label.contains("MusicBrainz: no-match"))
                        .count();
                    let ambiguous = library
                        .albums
                        .iter()
                        .filter(|a| a.source_label.contains("MusicBrainz: ambiguous"))
                        .count();
                    let mode = if self.network.load(Ordering::Acquire) {
                        "metadata checked"
                    } else {
                        "cached metadata checked; online disabled"
                    };
                    let files = self
                        .state
                        .progress
                        .and_then(|n| n.total)
                        .map(|n| format!("{n} files • "))
                        .unwrap_or_default();
                    self.notice(format!(
                        "{files}{} albums • {mode} • {unmatched} unmatched • {ambiguous} ambiguous",
                        library.albums.len()
                    ));
                }
            }
            Command::Preferences(preferences) => {
                self.state.preferences = preferences;
                match self.store.save_preferences(&self.state.preferences) {
                    Ok(()) => self.notice(
                        "Settings saved. Reopen for dump selection or new metadata lookups.".into(),
                    ),
                    Err(error) => self.notice(format!("Could not save settings: {error}")),
                }
            }
            Command::Persist(operation) => {
                operation()?;
            }
            Command::LocalCommit(commit) => {
                self.local_commit = Some(commit);
            }
            Command::External(v, data, commit, reply) => {
                if !self.current(v) {
                    let _ = reply.send(Ok(false));
                    return Ok(());
                }
                let undo = match commit() {
                    Ok(undo) => undo,
                    Err(error) => {
                        let _ = reply.send(Err(anyhow::anyhow!(error.to_string())));
                        return Err(error);
                    }
                };
                let previous_source = match self.store.active_source().and_then(|previous| {
                    self.store.save_active_source(&data.source)?;
                    Ok(previous)
                }) {
                    Ok(previous) => previous,
                    Err(error) => {
                        let failure = match undo() {
                            Ok(()) => error,
                            Err(undo_error) => {
                                error.context(format!("Selection rollback failed: {undo_error}"))
                            }
                        };
                        let _ = reply.send(Err(anyhow::anyhow!(failure.to_string())));
                        return Err(failure);
                    }
                };
                // Only admission/publication holds this lock. Persistence and
                // compensation never block a caller advancing the generation.
                let acceptance = self.generation.acceptance.clone();
                let accepted = acceptance.lock().unwrap();
                if self.current(v) {
                    self.replace(data.library, data.artwork, Source::External(data.source));
                    self.publish();
                    let _ = reply.send(Ok(true));
                } else {
                    drop(accepted);
                    self.store
                        .restore_active_source(previous_source.as_deref())?;
                    match undo() {
                        Ok(()) => {
                            let _ = reply.send(Ok(false));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(anyhow::anyhow!(error.to_string())));
                            return Err(error);
                        }
                    }
                }
            }
            Command::Read(reply) => {
                let _ = reply.send(self.state.clone());
            }
            _ => {}
        }
        Ok(())
    }
    fn enrich(&mut self, v: u64) -> Result<()> {
        if self.state.enriching {
            return Ok(());
        }
        self.state.enriching = true;
        let mut library = (*self.state.library).clone();
        let backend = self.backend.clone();
        let cache = self.cache.clone();
        let generation = self.generation.clone();
        let network = self.network.clone();
        let tx = self.generation.tx.clone();
        let spawned = thread::Builder::new()
            .name("amp-metadata".into())
            .spawn(move || {
                let result = Store::open(&cache).and_then(|store| {
                    backend.enrich(
                        &mut library,
                        &store,
                        &|| network.load(Ordering::Acquire),
                        &|| generation.current() != v,
                        &mut |album| {
                            let _ = tx.send(Command::Metadata(v, album.clone()));
                        },
                    )
                });
                let _ = tx.send(Command::Enriched(v, result));
            });
        if let Err(error) = spawned {
            self.state.enriching = false;
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
