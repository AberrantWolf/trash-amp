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
        id: id.into(),
        root: format!("/offline/{id}"),
        warnings: vec![],
        albums: vec![Album {
            id: identity.clone(),
            title: title.into(),
            artist: "Artist".into(),
            source_label: "Local".into(),
            tracks: vec![Track {
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
    handle.send(Command::Scanned(old, Ok(library("late", "Late scan"))));
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
