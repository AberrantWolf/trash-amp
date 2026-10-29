use super::*;
use crate::PlaybackService;
use amp_core::{Availability, ItemId, MediaAccess, Track};
use std::{
    sync::{Mutex, atomic::AtomicUsize},
    time::{Duration, Instant},
};

fn library(id: &str, title: &str) -> Library {
    let identity = ItemId {
        library: id.into(),
        provider: "test".into(),
        item: "album".into(),
    };
    Library {
        scan: None,
        id: id.into(),
        root: format!("/offline/{id}"),
        warnings: vec![],
        albums: vec![Album {
            metadata: Default::default(),
            id: identity.clone(),
            title: title.into(),
            artist: "Artist".into(),
            source_label: "Local".into(),
            tracks: vec![Track {
                scan_status: None,
                blocked_reason: None,
                id: ItemId {
                    item: "track".into(),
                    ..identity
                },
                availability: Availability::Available,
                source: None,
                title: title.into(),
                artist: "Artist".into(),
                disc: 1,
                number: 1,
                duration_seconds: 60.0,
                access: MediaAccess::Managed {
                    provider: "test".into(),
                    item: "opaque".into(),
                },
            }],
        }],
    }
}
struct FixtureBackend {
    result: Library,
    enrich: bool,
    fail_scan: bool,
}
impl Backend for FixtureBackend {
    fn scan(
        &self,
        _root: PathBuf,
        _preferences: Preferences,
        _cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(ScanProgress),
    ) -> Result<Library> {
        if self.fail_scan {
            anyhow::bail!("mount unavailable");
        }
        progress(ScanProgress {
            discovered: 1,
            completed: 1,
            total: Some(1),
        });
        Ok(self.result.clone())
    }
    fn enrich(
        &self,
        library: &mut Library,
        _store: &Store,
        _network: &dyn Fn() -> bool,
        _cancelled: &dyn Fn() -> bool,
        update: &mut dyn FnMut(&Album),
    ) -> Result<()> {
        if self.enrich {
            for album in &mut library.albums {
                album.title = "Enriched".into();
                album.tracks[0].title = "Enriched".into();
                update(album);
            }
        }
        Ok(())
    }
}
struct Harness {
    catalog: CatalogService,
    playback: PlaybackService,
    _directory: tempfile::TempDir,
    cache: PathBuf,
}
impl Harness {
    fn new(seed: &[Library], backend: Arc<dyn Backend>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("library.sqlite3");
        let store = Store::open(&cache).unwrap();
        for library in seed {
            store.save_library(library).unwrap();
        }
        drop(store);
        let playback = PlaybackService::new(Arc::new(|| Ok(()))).unwrap();
        let catalog = CatalogService::with_backend(&cache, playback.playback(), backend).unwrap();
        Self {
            catalog,
            playback,
            _directory: directory,
            cache,
        }
    }
    fn simple(seed: &[Library]) -> Self {
        Self::new(
            seed,
            Arc::new(FixtureBackend {
                result: library("scan", "Scanned"),
                enrich: false,
                fail_scan: false,
            }),
        )
    }
    fn handle(&self) -> Catalog {
        self.catalog.catalog()
    }
    fn store(&self) -> Store {
        Store::open(&self.cache).unwrap()
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "background catalog operation completes"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn read(catalog: &Catalog) -> Snapshot {
    catalog.bootstrap_snapshot().unwrap()
}

#[test]
fn partial_refresh_preserves_active_tracks_newer_than_the_saved_cache() {
    use amp_library::coverage::{Coverage, DirectoryCoverage};
    let mut saved = library("folder", "Saved song");
    saved.albums[0].tracks[0].access = MediaAccess::File {
        locator: format!("{}/saved.wav", saved.root),
    };
    let other = library("other", "Other folder");
    let harness = Harness::simple(&[saved.clone(), other.clone()]);
    let mut state = read(&harness.handle());
    state.source = Source::Local;
    let mut current = saved.clone();
    let mut added = current.albums[0].tracks[0].clone();
    added.id.item = "new-song".into();
    added.title = "New song whose cache write failed".into();
    added.access = MediaAccess::File {
        locator: format!("{}/new.wav", current.root),
    };
    current.albums[0].tracks.push(added);
    state.library = Arc::new(current);
    let store = harness.store();
    assert_eq!(
        store.library_by_id(&saved.id).unwrap().unwrap().albums[0]
            .tracks
            .len(),
        1
    );
    let baseline = scan_baseline(&state, &store, &saved.id).unwrap().unwrap();
    let coverage = Coverage {
        root: saved.root.clone().into(),
        directories: [(
            PathBuf::from(&saved.root),
            DirectoryCoverage {
                complete: false,
                children: Default::default(),
            },
        )]
        .into(),
        files: Default::default(),
        failures: Default::default(),
        mounts: Default::default(),
        issues: vec![amp_core::ScanIssue {
            path: saved.root.clone(),
            reason: "Permission denied".into(),
        }],
        source_disconnected: false,
        source_identity: None,
    };
    let refreshed = ScanResult {
        library: Library {
            id: saved.id.clone(),
            root: saved.root.clone(),
            ..Default::default()
        },
        coverage: Some(coverage),
    }
    .reconcile(Some(&baseline));
    assert_eq!(refreshed.albums[0].tracks.len(), 2);
    assert!(
        refreshed.albums[0]
            .tracks
            .iter()
            .any(|track| track.title == "New song whose cache write failed")
    );
    assert!(
        refreshed.albums[0]
            .tracks
            .iter()
            .all(|track| track.scan_status.is_some() && track.playback_block().is_none())
    );
    assert_eq!(
        scan_baseline(&state, &store, &other.id)
            .unwrap()
            .unwrap()
            .albums[0]
            .title,
        "Other folder"
    );
    state.source = Source::External("unrelated-provider".into());
    assert_eq!(
        scan_baseline(&state, &store, &saved.id)
            .unwrap()
            .unwrap()
            .albums[0]
            .tracks
            .len(),
        1
    );
}

#[test]
fn incomplete_refresh_reconciles_with_the_saved_folder_and_survives_restart() {
    use amp_library::coverage::{Coverage, DirectoryCoverage};
    struct CoveredBackend(Mutex<Option<ScanResult>>);
    impl Backend for CoveredBackend {
        fn scan(
            &self,
            _: PathBuf,
            _: Preferences,
            _: &dyn Fn() -> bool,
            _: &mut dyn FnMut(ScanProgress),
        ) -> Result<Library> {
            unreachable!("the owner must request coverage")
        }
        fn scan_with_coverage(
            &self,
            _: PathBuf,
            _: Preferences,
            _: &dyn Fn() -> bool,
            _: &mut dyn FnMut(ScanProgress),
        ) -> Result<ScanResult> {
            Ok(self.0.lock().unwrap().take().unwrap())
        }
        fn enrich(
            &self,
            _: &mut Library,
            _: &Store,
            _: &dyn Fn() -> bool,
            _: &dyn Fn() -> bool,
            _: &mut dyn FnMut(&Album),
        ) -> Result<()> {
            Ok(())
        }
    }
    let mut saved = library("folder", "Known song");
    let path = PathBuf::from(&saved.root).join("album/song.wav");
    saved.albums[0].tracks[0].access = MediaAccess::File {
        locator: path.to_string_lossy().into(),
    };
    let coverage = Coverage {
        mounts: Default::default(),
        root: saved.root.clone().into(),
        directories: [(
            PathBuf::from(&saved.root),
            DirectoryCoverage {
                complete: false,
                children: Default::default(),
            },
        )]
        .into(),
        files: Default::default(),
        failures: Default::default(),
        issues: vec![amp_core::ScanIssue {
            path: saved.root.clone(),
            reason: "Permission denied".into(),
        }],
        source_disconnected: false,
        source_identity: Some("fixture-source".into()),
    };
    let scan = ScanResult {
        library: Library {
            id: saved.id.clone(),
            root: saved.root.clone(),
            ..Default::default()
        },
        coverage: Some(coverage),
    };
    let harness = Harness::new(
        &[saved.clone()],
        Arc::new(CoveredBackend(Mutex::new(Some(scan)))),
    );
    harness.handle().scan_folder(saved.root.clone().into());
    wait(|| {
        harness
            .store()
            .library()
            .unwrap()
            .is_some_and(|library| library.scan.is_some())
    });
    let reopened = harness.store().library().unwrap().unwrap();
    let track = &reopened.albums[0].tracks[0];
    assert_eq!(track.title, "Known song");
    assert!(track.scan_status.is_some());
    assert!(track.playback_block().is_none());
    assert_eq!(reopened.scan.as_ref().unwrap().removed, 0);
    assert!(reopened.scan.as_ref().unwrap().incomplete);
    let snapshot = read(&harness.handle());
    assert!(snapshot.library.scan.as_ref().unwrap().incomplete);
    let cache = harness.cache.clone();
    drop(harness.catalog);
    let playback = PlaybackService::new(Arc::new(|| Ok(()))).unwrap();
    let restarted = CatalogService::open(&cache, playback.playback()).unwrap();
    let snapshot = restarted.catalog().bootstrap_snapshot().unwrap();
    assert!(snapshot.library.albums[0].tracks[0].scan_status.is_some());
    assert!(snapshot.library.scan.as_ref().unwrap().incomplete);
}

#[test]
fn scan_enrichment_and_queue_metadata_persist_without_a_presentation_consumer() {
    let scanned = library("scan", "Scanned");
    let harness = Harness::new(
        &[],
        Arc::new(FixtureBackend {
            result: scanned.clone(),
            enrich: true,
            fail_scan: false,
        }),
    );
    let playback = harness.playback.playback();
    playback.enqueue(scanned.albums[0].tracks[0].clone());
    harness
        .handle()
        .scan_folder(PathBuf::from("opaque-test-mount"));
    let store = harness.store();
    // No catalog snapshots or presentation events are consumed to drive either commit.
    wait(|| {
        store
            .library()
            .unwrap()
            .is_some_and(|library| library.albums[0].title == "Enriched")
    });
    wait(|| {
        playback
            .snapshot()
            .up_next
            .first()
            .is_some_and(|t| t.title == "Enriched")
    });
    assert_eq!(store.saved_folders().unwrap().len(), 1);
    let snapshot = read(&harness.handle());
    assert!(!snapshot.scanning && !snapshot.enriching);
    assert_eq!(snapshot.library.id, "scan");
}

#[test]
fn failed_scan_keeps_the_saved_catalog_and_selection() {
    let saved = library("saved", "Saved");
    let harness = Harness::new(
        &[saved],
        Arc::new(FixtureBackend {
            result: library("bad", "Bad"),
            enrich: false,
            fail_scan: true,
        }),
    );
    let handle = harness.handle();
    handle.scan_folder(PathBuf::from("missing"));
    wait(|| {
        handle
            .snapshot()
            .notice
            .is_some_and(|n| n.message.starts_with("Library unchanged"))
    });
    assert_eq!(read(&handle).library.id, "saved");
    assert_eq!(harness.store().library().unwrap().unwrap().id, "saved");
    assert!(harness.store().library_by_id("bad").unwrap().is_none());
}

#[test]
fn source_switch_flushes_accepted_metadata_but_rejects_late_scan_and_metadata() {
    let a = library("a", "Original");
    let b = library("b", "Other");
    let harness = Harness::simple(&[b.clone(), a.clone()]);
    let handle = harness.handle();
    let old = handle.generation().current();
    let accepted = library("a", "Accepted").albums.remove(0);
    handle.send(Command::Metadata(old, accepted));
    read(&handle);
    handle.open_saved_folder("b".into());
    assert_eq!(read(&handle).library.id, "b");
    assert_eq!(
        harness.store().library_by_id("a").unwrap().unwrap().albums[0].title,
        "Accepted"
    );
    // Simulate a scanner and an in-flight lookup returning after the selection changed.
    handle.send(Command::Scanned(
        old,
        Ok(ScanResult::complete(library("late", "Late scan"))),
    ));
    handle.send(Command::Metadata(
        old,
        library("a", "Late metadata").albums.remove(0),
    ));
    handle.send(Command::Enriched(old, Ok(())));
    let snapshot = read(&handle);
    assert_eq!(snapshot.library.id, "b");
    assert_eq!(harness.store().library().unwrap().unwrap().id, "b");
    assert!(harness.store().library_by_id("late").unwrap().is_none());
    assert_eq!(
        harness.store().library_by_id("a").unwrap().unwrap().albums[0].title,
        "Accepted"
    );
}

#[test]
fn external_completion_commits_without_ui_and_never_becomes_a_folder_library() {
    let harness = Harness::simple(&[library("local", "Local")]);
    let handle = harness.handle();
    let commits = Arc::new(AtomicUsize::new(0));
    let commit = commits.clone();
    let v = handle.generation().advance();
    let mut remote = library("remote", "Remote");
    remote.root = "content://provider/non-filesystem-catalog".into();
    let accepted = handle.complete_external(
        v,
        ExternalCatalog {
            online: None,
            source: "remote-source".into(),
            library: remote,
            artwork: BTreeMap::new(),
        },
        Arc::new(move || {
            commit.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(|| Ok(())))
        }),
    );
    assert!(
        accepted
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        read(&handle).source,
        Source::External("remote-source".into())
    );
    assert_eq!(harness.store().saved_folders().unwrap().len(), 1);
    assert_eq!(harness.store().library().unwrap().unwrap().id, "local");
    handle.restore_local();
    read(&handle);
    let commit = commits.clone();
    let stale = handle.complete_external(
        v,
        ExternalCatalog {
            online: None,
            source: "stale".into(),
            library: library("late", "Late"),
            artwork: BTreeMap::new(),
        },
        Arc::new(move || {
            commit.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(|| Ok(())))
        }),
    );
    assert!(!stale.recv_timeout(Duration::from_secs(2)).unwrap().unwrap());
    assert_eq!(
        commits.load(Ordering::SeqCst),
        1,
        "a stale provider cannot reselect itself"
    );
    assert_eq!(read(&handle).library.id, "local");
}

#[test]
fn rejected_provider_persistence_does_not_install_its_catalog() {
    let harness = Harness::simple(&[library("local", "Local")]);
    let handle = harness.handle();
    let v = handle.generation().advance();
    let result = handle.complete_external(
        v,
        ExternalCatalog {
            online: None,
            source: "remote".into(),
            library: library("remote", "Remote"),
            artwork: BTreeMap::new(),
        },
        Arc::new(|| anyhow::bail!("selection write failed")),
    );
    assert!(
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err()
    );
    let snapshot = read(&handle);
    assert_eq!(snapshot.source, Source::Local);
    assert_eq!(snapshot.library.id, "local");
    assert!(
        snapshot
            .notice
            .unwrap()
            .message
            .contains("selection write failed")
    );
}

#[test]
fn failed_scan_from_external_source_restores_the_local_cache_before_scanning() {
    let harness = Harness::new(
        &[library("saved", "Saved")],
        Arc::new(FixtureBackend {
            result: Library::default(),
            enrich: false,
            fail_scan: true,
        }),
    );
    let handle = harness.handle();
    let selected = Arc::new(AtomicUsize::new(0));
    let local = selected.clone();
    handle.on_local_selection(Arc::new(move || {
        local.store(0, Ordering::SeqCst);
        Ok(Box::new(|| Ok(())))
    }));
    let remote = selected.clone();
    let v = handle.generation().advance();
    handle
        .complete_external(
            v,
            ExternalCatalog {
                online: None,
                source: "remote".into(),
                library: library("remote", "Remote"),
                artwork: BTreeMap::new(),
            },
            Arc::new(move || {
                remote.store(1, Ordering::SeqCst);
                Ok(Box::new(|| Ok(())))
            }),
        )
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    assert_eq!(selected.load(Ordering::SeqCst), 1);
    handle.scan_folder(PathBuf::from("missing"));
    wait(|| {
        handle
            .snapshot()
            .notice
            .is_some_and(|n| n.message.starts_with("Library unchanged"))
    });
    assert_eq!(read(&handle).library.id, "saved");
    assert_eq!(read(&handle).source, Source::Local);
    assert_eq!(selected.load(Ordering::SeqCst), 0);
}

struct NetworkBackend {
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    observed: mpsc::Sender<bool>,
}
impl Backend for NetworkBackend {
    fn scan(
        &self,
        _root: PathBuf,
        _preferences: Preferences,
        _cancelled: &dyn Fn() -> bool,
        _progress: &mut dyn FnMut(ScanProgress),
    ) -> Result<Library> {
        unreachable!()
    }
    fn enrich(
        &self,
        _library: &mut Library,
        _store: &Store,
        network: &dyn Fn() -> bool,
        _cancelled: &dyn Fn() -> bool,
        _update: &mut dyn FnMut(&Album),
    ) -> Result<()> {
        self.started.send(())?;
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))?;
        self.observed.send(network())?;
        Ok(())
    }
}
#[test]
fn disabling_metadata_takes_effect_even_while_persistence_is_busy() {
    let (started, started_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let (observed, observed_rx) = mpsc::channel();
    let harness = Harness::new(
        &[library("saved", "Saved")],
        Arc::new(NetworkBackend {
            started,
            release: Mutex::new(release_rx),
            observed,
        }),
    );
    let handle = harness.handle();
    handle.enrich_current();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (busy, busy_rx) = mpsc::channel();
    let (unblock, unblock_rx) = mpsc::channel();
    handle.persist(Box::new(move || {
        busy.send(())?;
        unblock_rx.recv_timeout(Duration::from_secs(2))?;
        Ok(())
    }));
    busy_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let preferences = Preferences {
        automatic_metadata: false,
        text_scale: 1.5,
        ..Default::default()
    };
    handle.save_preferences(preferences);
    release.send(()).unwrap();
    assert!(
        !observed_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "no new lookup should wait for a database commit to stop"
    );
    unblock.send(()).unwrap();
    read(&handle);
    let preferences = harness.store().preferences().unwrap();
    assert!(!preferences.automatic_metadata);
    assert_eq!(preferences.text_scale, 1.5);
}

#[test]
fn invalid_saved_folder_retains_the_current_library_and_persistent_selection() {
    let harness = Harness::simple(&[library("saved", "Saved")]);
    let handle = harness.handle();
    handle.open_saved_folder("missing".into());
    let snapshot = read(&handle);
    assert_eq!(snapshot.library.id, "saved");
    assert!(snapshot.notice.unwrap().message.contains("unavailable"));
    assert_eq!(harness.store().library().unwrap().unwrap().id, "saved");
}

#[test]
fn accepted_external_catalog_updates_queued_names_without_ui_or_media_replacement() {
    let harness = Harness::simple(&[]);
    let handle = harness.handle();
    let mut original = library("remote", "Original");
    let playback = harness.playback.playback();
    playback.enqueue(original.albums[0].tracks[0].clone());
    original.albums[0].tracks[0].title = "Refreshed".into();
    original.albums[0].tracks[0].access = MediaAccess::Managed {
        provider: "test".into(),
        item: "changed-access".into(),
    };
    let v = handle.generation().advance();
    handle
        .complete_external(
            v,
            ExternalCatalog {
                online: None,
                source: "remote".into(),
                library: original,
                artwork: BTreeMap::new(),
            },
            Arc::new(|| Ok(Box::new(|| Ok(())))),
        )
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    wait(|| {
        playback
            .snapshot()
            .up_next
            .first()
            .is_some_and(|t| t.title == "Refreshed")
    });
    assert!(
        matches!(&playback.snapshot().up_next[0].access, MediaAccess::Managed { item, .. } if item == "opaque")
    );
}

#[test]
fn background_source_replacement_clears_an_idle_play_selection_from_the_previous_source() {
    let directory = tempfile::tempdir().unwrap();
    let attempted = Arc::new(AtomicUsize::new(0));
    let count = attempted.clone();
    let playback_owner = PlaybackService::new(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("test must not open an output device");
    }))
    .unwrap();
    let playback = playback_owner.playback();
    let old = library("old", "Old");
    playback.set_default_album(Some((old.albums[0].tracks.clone(), false)));
    let service =
        CatalogService::open(&directory.path().join("library.sqlite3"), playback.clone()).unwrap();
    let handle = service.catalog();
    let v = handle.generation().advance();
    handle
        .complete_external(
            v,
            ExternalCatalog {
                online: None,
                source: "new".into(),
                library: library("new", "New"),
                artwork: BTreeMap::new(),
            },
            Arc::new(|| Ok(Box::new(|| Ok(())))),
        )
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    playback.transport(crate::Transport::Play);
    let (tx, rx) = mpsc::channel();
    playback.send(crate::Command::Barrier(tx));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(attempted.load(Ordering::SeqCst), 0);
    assert!(playback.snapshot().current.is_none());
}

#[test]
fn cancellation_during_external_persistence_rolls_back_selection_and_rejects_catalog() {
    let harness = Harness::simple(&[library("local", "Local")]);
    let handle = harness.handle();
    let selected = Arc::new(AtomicUsize::new(0));
    let persisted = selected.clone();
    let (entered, waiting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    let v = handle.generation().advance();
    let receipt = handle.complete_external(
        v,
        ExternalCatalog {
            online: None,
            source: "canceled".into(),
            library: library("canceled", "Canceled"),
            artwork: BTreeMap::new(),
        },
        Arc::new(move || {
            let previous = persisted.swap(1, Ordering::SeqCst);
            entered.send(()).unwrap();
            gate.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            let persisted = persisted.clone();
            Ok(Box::new(move || {
                persisted.store(previous, Ordering::SeqCst);
                Ok(())
            }))
        }),
    );
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    // This must return while persistence is blocked, without a UI-thread wait.
    let next = handle.generation().advance();
    release.send(()).unwrap();
    assert!(
        !receipt
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
    );
    let state = read(&handle);
    assert_eq!(state.generation, next);
    assert_eq!(state.source, Source::Local);
    assert_eq!(state.library.id, "local");
    assert_eq!(selected.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_during_local_selection_restores_provider_and_folder_selection() {
    let harness = Harness::simple(&[library("a", "A"), library("b", "B")]);
    let handle = harness.handle();
    let selected = Arc::new(AtomicUsize::new(1));
    let persisted = selected.clone();
    let (entered, waiting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    handle.on_local_selection(Arc::new(move || {
        let previous = persisted.swap(0, Ordering::SeqCst);
        entered.send(()).unwrap();
        gate.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let persisted = persisted.clone();
        Ok(Box::new(move || {
            persisted.store(previous, Ordering::SeqCst);
            Ok(())
        }))
    }));
    handle.open_saved_folder("a".into());
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    handle.generation().advance();
    release.send(()).unwrap();
    assert_eq!(read(&handle).library.id, "b");
    assert_eq!(harness.store().library().unwrap().unwrap().id, "b");
    assert_eq!(selected.load(Ordering::SeqCst), 1);
}

struct TestProvider {
    calls: AtomicUsize,
    allowed: AtomicBool,
    fail: AtomicBool,
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    changed: Mutex<Option<crate::sources::SourceObserver>>,
}
impl TestProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            allowed: AtomicBool::new(true),
            fail: AtomicBool::new(false),
            gate: Mutex::default(),
            changed: Mutex::default(),
        })
    }
}
impl SourceProvider for TestProvider {
    fn descriptors(&self) -> Result<Vec<SourceDescriptor>> {
        Ok(vec![SourceDescriptor {
            id: "native:test".into(),
            name: "Native music".into(),
            provider: "native".into(),
        }])
    }
    fn cache_allowed(&self) -> bool {
        self.allowed.load(Ordering::Acquire)
    }
    fn load(&self, _: &str, _: bool, _: &dyn Fn() -> bool) -> Result<ExternalCatalog> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(gate) = self.gate.lock().unwrap().take() {
            gate.recv_timeout(Duration::from_secs(3))?;
        }
        if !self.cache_allowed() {
            anyhow::bail!("Access denied");
        }
        if self.fail.load(Ordering::Acquire) {
            anyhow::bail!("Refresh unavailable");
        }
        Ok(ExternalCatalog {
            online: None,
            source: "native:test".into(),
            library: library("native", "Native music"),
            artwork: BTreeMap::new(),
        })
    }
    fn observe(&self, changed: crate::sources::SourceObserver) {
        *self.changed.lock().unwrap() = Some(changed);
    }
}
#[test]
fn registered_sources_load_persist_and_fail_refresh_without_ui() {
    let h = Harness::simple(&[]);
    let catalog = h.handle();
    let provider = TestProvider::new();
    catalog.register_sources(vec![provider.clone()], false);
    assert_eq!(read(&catalog).sources.len(), 1);
    catalog.select_source("native:test".into());
    wait(|| catalog.snapshot().library.id == "native" && !catalog.snapshot().scanning);
    assert_eq!(
        h.store().active_source().unwrap().as_deref(),
        Some("native:test")
    );
    assert!(h.store().saved_folders().unwrap().is_empty());
    provider.fail.store(true, Ordering::Release);
    catalog.refresh_source();
    wait(|| {
        catalog
            .snapshot()
            .notice
            .is_some_and(|n| n.message.contains("Refresh unavailable"))
    });
    assert_eq!(catalog.snapshot().library.id, "native");
    assert_eq!(
        h.store().active_source().unwrap().as_deref(),
        Some("native:test")
    );
}
#[test]
fn provider_completion_cannot_reselect_after_folder_switch() {
    let h = Harness::simple(&[library("folder", "Folder")]);
    let catalog = h.handle();
    let provider = TestProvider::new();
    let (release, gate) = mpsc::channel();
    *provider.gate.lock().unwrap() = Some(gate);
    catalog.register_sources(vec![provider.clone()], true);
    read(&catalog);
    catalog.select_source("native:test".into());
    wait(|| provider.calls.load(Ordering::Acquire) == 1);
    catalog.open_saved_folder("folder".into());
    read(&catalog);
    release.send(()).unwrap();
    // Barrier after the asynchronous completion is sent, not a presentation tick.
    wait(|| provider.gate.try_lock().is_ok());
    assert_eq!(read(&catalog).source, Source::Local);
    assert_eq!(
        h.store().active_source().unwrap().as_deref(),
        Some("folders")
    );
    assert_eq!(read(&catalog).library.id, "folder");
}
#[test]
fn provider_notification_revokes_cached_catalog_without_ui() {
    let h = Harness::simple(&[]);
    let catalog = h.handle();
    let provider = TestProvider::new();
    catalog.register_sources(vec![provider.clone()], false);
    read(&catalog);
    catalog.select_source("native:test".into());
    wait(|| catalog.snapshot().library.id == "native" && !catalog.snapshot().scanning);
    provider.allowed.store(false, Ordering::Release);
    provider.changed.lock().unwrap().as_ref().unwrap()(SourceChange {
        id: "native:test".into(),
        refresh: RefreshKind::Live,
    });
    wait(|| {
        catalog
            .snapshot()
            .notice
            .is_some_and(|n| n.message.contains("Access denied"))
    });
    assert!(catalog.snapshot().library.albums.is_empty());
    assert!(
        h.store()
            .provider_snapshot("native:test")
            .unwrap()
            .is_none()
    );
}

#[test]
fn registered_source_restores_its_cache_when_live_refresh_fails() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("library.sqlite3");
    let store = Store::open(&cache).unwrap();
    store.save_active_source("native:test").unwrap();
    store
        .save_provider_snapshot(
            "native:test",
            &library("native", "Cached native music"),
            &BTreeMap::new(),
        )
        .unwrap();
    drop(store);
    let playback = PlaybackService::new(Arc::new(|| Ok(()))).unwrap();
    let service = CatalogService::open(&cache, playback.playback()).unwrap();
    let provider = TestProvider::new();
    provider.fail.store(true, Ordering::Release);
    let catalog = service.catalog();
    catalog.register_sources(vec![provider], false);
    wait(|| {
        catalog
            .snapshot()
            .notice
            .is_some_and(|n| n.message.contains("Refresh unavailable"))
    });
    let snapshot = catalog.snapshot();
    assert_eq!(snapshot.source, Source::External("native:test".into()));
    assert_eq!(snapshot.library.albums[0].title, "Cached native music");
    assert!(snapshot.folders.is_empty());
}

#[test]
fn changes_during_loading_are_coalesced_and_reloaded_without_changing_selection_generation() {
    let h = Harness::simple(&[]);
    let catalog = h.handle();
    let provider = TestProvider::new();
    let (release, gate) = mpsc::channel();
    *provider.gate.lock().unwrap() = Some(gate);
    catalog.register_sources(vec![provider.clone()], false);
    read(&catalog);
    catalog.select_source("native:test".into());
    let version = catalog.generation().current();
    wait(|| provider.calls.load(Ordering::Acquire) == 1);
    for _ in 0..8 {
        provider.changed.lock().unwrap().as_ref().unwrap()(SourceChange {
            id: "native:test".into(),
            refresh: RefreshKind::Live,
        });
    }
    read(&catalog);
    release.send(()).unwrap();
    wait(|| provider.calls.load(Ordering::Acquire) == 2 && !catalog.snapshot().scanning);
    assert_eq!(
        catalog.generation().current(),
        version,
        "An automatic reload must not cancel a pending mutation"
    );
    assert_eq!(read(&catalog).source_load_revision, 2);
    assert_eq!(provider.calls.load(Ordering::Acquire), 2);
}

#[test]
fn old_request_artwork_is_rejected_even_within_the_same_selection_generation() {
    let h = Harness::simple(&[]);
    let catalog = h.handle();
    let provider = TestProvider::new();
    catalog.register_sources(vec![provider.clone()], false);
    read(&catalog);
    catalog.select_source("native:test".into());
    wait(|| provider.calls.load(Ordering::Acquire) == 1 && !catalog.snapshot().scanning);
    let version = catalog.generation().current();
    catalog.open_source_at(version, "native:test".into(), false);
    wait(|| provider.calls.load(Ordering::Acquire) == 2 && !catalog.snapshot().scanning);
    catalog.send(Command::SourceArtwork(
        SourceRequest {
            generation: version,
            serial: 2,
            id: "native:test".into(),
        },
        BTreeMap::from([("album".into(), "current.png".into())]),
    ));
    read(&catalog);
    catalog.send(Command::SourceArtwork(
        SourceRequest {
            generation: version,
            serial: 1,
            id: "native:test".into(),
        },
        BTreeMap::from([("album".into(), "obsolete.png".into())]),
    ));
    assert_eq!(read(&catalog).artwork.get("album").unwrap(), "current.png");
}

#[test]
fn availability_projection_does_not_fetch_live_data_or_cancel_mutation_generation() {
    struct Projection {
        live_calls: AtomicUsize,
    }
    impl SourceProvider for Projection {
        fn descriptors(&self) -> Result<Vec<SourceDescriptor>> {
            Ok(vec![SourceDescriptor {
                id: "test:network".into(),
                name: "Network".into(),
                provider: "test".into(),
            }])
        }
        fn load(&self, _: &str, _: bool, _: &dyn Fn() -> bool) -> Result<ExternalCatalog> {
            self.live_calls.fetch_add(1, Ordering::AcqRel);
            Ok(ExternalCatalog {
                source: "test:network".into(),
                online: Some(true),
                library: library("network", "Network"),
                artwork: BTreeMap::new(),
            })
        }
        fn project_cached(
            &self,
            id: &str,
            online: Option<bool>,
        ) -> Result<Option<ExternalCatalog>> {
            Ok(Some(ExternalCatalog {
                source: id.into(),
                online,
                library: library("network", "Download settled"),
                artwork: BTreeMap::new(),
            }))
        }
    }
    let h = Harness::simple(&[]);
    let catalog = h.handle();
    let provider = Arc::new(Projection {
        live_calls: AtomicUsize::new(0),
    });
    catalog.register_sources(vec![provider.clone()], false);
    read(&catalog);
    catalog.select_source("test:network".into());
    wait(|| catalog.snapshot().source_online == Some(true) && !catalog.snapshot().scanning);
    let version = catalog.generation().current();
    catalog.source_changed(SourceChange {
        id: "test:network".into(),
        refresh: RefreshKind::Cached,
    });
    wait(|| {
        catalog.snapshot().library.albums[0].title == "Download settled"
            && !catalog.snapshot().scanning
    });
    assert_eq!(provider.live_calls.load(Ordering::Acquire), 1);
    assert_eq!(catalog.generation().current(), version);
    assert_eq!(catalog.snapshot().source_online, Some(true));
}

#[test]
fn missing_matching_key_is_visible_in_the_matching_snapshot_and_can_be_retried() {
    let fixture = Harness::simple(&[library("local", "Untagged")]);
    let catalog = fixture.handle();
    let initial = read(&catalog);
    let id = initial.library.albums[0].id.clone();
    catalog.find_album_matches(id.clone());
    wait(|| catalog.snapshot().matching_error.is_some());
    let failed = read(&catalog);
    assert!(!failed.matching_busy);
    assert!(
        failed
            .matching_error
            .as_deref()
            .unwrap()
            .contains("AcoustID")
    );
    catalog.find_album_matches(id);
    wait(|| catalog.snapshot().matching_version > failed.matching_version);
    assert!(!read(&catalog).matching_busy);
}
