//! Catalog jobs, accepted completion and persistence. No UI event loop is needed.
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
    pub scanning: bool,
    pub enriching: bool,
    pub progress: Option<ScanProgress>,
    pub notice: Option<Notice>,
}
/// A provider's catalog is not assumed to be a filesystem hierarchy.
pub struct ExternalCatalog {
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
        self.state.library_revision += 1;
        self.state.replacement_revision += 1;
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
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Invalidate(v) if self.current(v) => {
                let flushed = self.flush_metadata();
                self.state.generation = v;
                self.state.scanning = false;
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
