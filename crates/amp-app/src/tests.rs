use super::*;
use amp_core::{Availability, ItemId, MediaAccess};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct FakeState {
    clock: amp_audio::Snapshot,
    tracks: Vec<Track>,
    events: VecDeque<PlayerEvent>,
    replacements: usize,
    processing: AudioProcessingSettings,
    processing_at_play: AudioProcessingSettings,
    dropped: bool,
    hold_snapshot: bool,
    next_sequence: u64,
    pause_leases: Arc<AtomicUsize>,
    edit_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    resume_gate_counts: Vec<usize>,
}
struct Fake(Arc<Mutex<FakeState>>);
struct TestPauseLease(Arc<AtomicUsize>);
impl Drop for TestPauseLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
impl Engine for Fake {
    fn render_pause(&self) -> Option<amp_audio::RenderPause> {
        let leases = self.0.lock().unwrap().pause_leases.clone();
        Some(Arc::new(move || {
            leases.fetch_add(1, Ordering::AcqRel);
            Box::new(TestPauseLease(leases.clone()))
        }))
    }

    fn set_processing(&self, settings: AudioProcessingSettings) {
        self.0.lock().unwrap().processing = settings;
    }
    fn play(&self, tracks: Vec<Track>, start: usize) -> u64 {
        let mut state = self.0.lock().unwrap();
        state.processing_at_play = state.processing;
        state.replacements += 1;
        state.next_sequence += 1;
        state.tracks = tracks;
        if !state.hold_snapshot {
            state.clock = amp_audio::Snapshot {
                sequence: state.next_sequence,
                track: start,
                playing: true,
                output: OutputState::Ready,
                ..Default::default()
            };
        }
        state.next_sequence
    }
    fn set_playing(&self, playing: bool) {
        let mut state = self.0.lock().unwrap();
        if playing {
            let count = state.pause_leases.load(Ordering::Acquire);
            state.resume_gate_counts.push(count);
        }
        state.clock.playing = playing;
    }
    fn toggle(&self) {
        let mut s = self.0.lock().unwrap();
        s.clock.playing = !s.clock.playing;
    }
    fn seek(&self, seconds: f64) {
        self.0.lock().unwrap().clock.seconds = seconds;
    }
    fn next(&self) {
        let mut s = self.0.lock().unwrap();
        s.clock.track += 1;
        s.clock.seconds = 0.0;
    }
    fn previous(&self) {
        let mut s = self.0.lock().unwrap();
        if s.clock.seconds > 3.0 {
            s.clock.seconds = 0.0;
        } else {
            s.clock.track = s.clock.track.saturating_sub(1);
        }
    }
    fn append(&self, tracks: Vec<Track>) {
        self.0.lock().unwrap().tracks.extend(tracks);
    }
    fn truncate(&self, len: usize) {
        self.0.lock().unwrap().tracks.truncate(len);
    }
    fn edit_queue(&self, edit: amp_audio::QueueEdit) -> Result<amp_audio::QueueUpdate> {
        let mut state = self.0.lock().unwrap();
        if let Some((entered, release)) = state.edit_gate.take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let (tracks, start) = edit(state.clock.track);
        state.tracks = tracks.clone();
        state.next_sequence += 1;
        state.clock.sequence = state.next_sequence;
        if let Some(start) = start {
            state.clock.track = start;
            state.clock.seconds = 0.0;
            state.clock.playing = true;
        }
        state.clock.playing &= state.clock.track < tracks.len();
        Ok(amp_audio::QueueUpdate {
            tracks,
            clock: state.clock.clone(),
        })
    }
    fn snapshot(&self) -> amp_audio::Snapshot {
        self.0.lock().unwrap().clock.clone()
    }
    fn event(&self) -> Option<PlayerEvent> {
        self.0.lock().unwrap().events.pop_front()
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.0.lock().unwrap().dropped = true;
    }
}
fn track(name: &str) -> Track {
    Track {
        scan_status: None,
        blocked_reason: None,
        id: ItemId {
            library: "test".into(),
            provider: "test".into(),
            item: name.into(),
        },
        availability: Availability::Available,
        source: None,
        title: name.into(),
        artist: "Artist".into(),
        number: 1,
        disc: 1,
        duration_seconds: 100.0,
        access: MediaAccess::File {
            locator: format!("/{name}.wav"),
        },
    }
}
fn setup() -> (PlaybackService, Arc<Mutex<FakeState>>) {
    setup_with(Arc::new(|| Ok(())))
}
fn setup_with(prepare: PreparePlayback) -> (PlaybackService, Arc<Mutex<FakeState>>) {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let fake = state.clone();
    let service = PlaybackService::with_engine_factory(
        prepare,
        Box::new(move |_, _| Ok(Box::new(Fake(fake.clone())))),
    )
    .unwrap();
    (service, state)
}
fn settle(playback: &Playback) -> Snapshot {
    let (tx, rx) = mpsc::channel();
    playback.send(Command::Barrier(tx));
    rx.recv_timeout(Duration::from_secs(2))
        .expect("application worker responds");
    playback.snapshot()
}
fn ids(tracks: &[Track]) -> Vec<&str> {
    tracks.iter().map(|t| t.id.item.as_str()).collect()
}

#[test]
fn native_transport_and_track_transitions_publish_without_ui_polling() {
    let (service, fake) = setup();
    let playback = service.playback();
    let (tx, rx) = mpsc::channel();
    playback.observe_now_playing(Some(Arc::new(move |state| {
        let _ = tx.send(state);
    })));
    playback.set_default_album(Some((vec![track("one"), track("two")], false)));
    playback.transport(Transport::Play);
    loop {
        let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        if event.track.as_ref().is_some_and(|t| t.id.item == "one") && event.playing {
            break;
        }
    }
    // Simulate the engine crossing a track boundary with no view and no UI reads.
    {
        let mut state = fake.lock().unwrap();
        state.clock.track = 1;
        state.clock.seconds = 4.0;
    }
    loop {
        let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        if event.track.as_ref().is_some_and(|t| t.id.item == "two") {
            assert_eq!(event.seconds, 4.0);
            break;
        }
    }
    playback.transport(Transport::Pause);
    assert!(!settle(&playback).playing);
    playback.transport(Transport::SeekFraction(0.25));
    assert_eq!(settle(&playback).seconds, 25.0);
    playback.transport(Transport::Stop);
    let stopped = settle(&playback);
    assert!(!stopped.playing);
    assert_eq!(stopped.seconds, 0.0);
}

#[test]
fn single_track_keeps_only_explicit_queue_and_clear_preserves_album_tail() {
    let (service, fake) = setup();
    let p = service.playback();
    p.enqueue(track("pending"));
    assert_eq!(ids(&settle(&p).up_next), ["pending"]);
    p.play_album(vec![track("one"), track("two"), track("three")], 0);
    assert_eq!(ids(&settle(&p).up_next), ["pending"]);
    p.enqueue(track("explicit"));
    assert_eq!(ids(&settle(&p).up_next), ["pending", "explicit"]);
    p.play_track(track("search-result"));
    let snapshot = settle(&p);
    assert_eq!(snapshot.current.unwrap().id.item, "search-result");
    assert_eq!(ids(&snapshot.up_next), ["pending", "explicit"]);
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["search-result", "pending", "explicit"]
    );
    p.play_album(vec![track("album-one"), track("album-two")], 0);
    p.enqueue(track("remove-me"));
    p.clear_queue();
    assert!(settle(&p).up_next.is_empty());
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["album-one", "album-two"]
    );
}

#[test]
fn list_replacement_discards_explicit_queue_and_clear_keeps_current_explicit_track() {
    let (service, fake) = setup();
    let p = service.playback();
    p.enqueue(track("pending"));
    p.play_list(vec![track("list-one"), track("list-two")]);
    assert!(settle(&p).up_next.is_empty());
    p.enqueue(track("current-explicit"));
    p.enqueue(track("later"));
    settle(&p);
    fake.lock().unwrap().clock.track = 2;
    settle(&p);
    p.clear_queue();
    assert!(settle(&p).up_next.is_empty());
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["list-one", "list-two", "current-explicit"]
    );
}

#[test]
fn failed_session_activation_preserves_queue_and_allows_retry() {
    let fail = Arc::new(AtomicBool::new(true));
    let gate = fail.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (service, fake) = setup_with(Arc::new(move || {
        count.fetch_add(1, Ordering::Relaxed);
        if gate.load(Ordering::Relaxed) {
            bail!("session unavailable");
        }
        Ok(())
    }));
    let p = service.playback();
    p.enqueue(track("pending"));
    p.set_default_album(Some((vec![track("album")], false)));
    p.transport(Transport::Play);
    let failed = settle(&p);
    assert!(failed.current.is_none());
    assert_eq!(ids(&failed.up_next), ["pending"]);
    assert!(
        failed
            .notice
            .unwrap()
            .message
            .contains("session unavailable")
    );
    assert_eq!(fake.lock().unwrap().replacements, 0);
    fail.store(false, Ordering::Relaxed);
    p.transport(Transport::Play);
    assert_eq!(settle(&p).current.unwrap().id.item, "album");
    fail.store(true, Ordering::Relaxed);
    p.play_track(track("replacement"));
    let unchanged = settle(&p);
    assert_eq!(unchanged.current.unwrap().id.item, "album");
    assert_eq!(ids(&unchanged.up_next), ["pending"]);
    fail.store(false, Ordering::Relaxed);
    p.transport(Transport::Pause);
    settle(&p);
    p.transport(Transport::Play);
    assert!(settle(&p).playing);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        4,
        "resume must activate the session again"
    );
}

#[test]
fn ambiguous_default_does_not_start_and_replacing_catalog_does_not_stop_audio() {
    let (service, fake) = setup();
    let p = service.playback();
    p.set_default_album(Some((vec![track("ambiguous")], true)));
    p.transport(Transport::Toggle);
    assert!(
        settle(&p)
            .notice
            .unwrap()
            .message
            .contains("Choose a source disc")
    );
    assert_eq!(fake.lock().unwrap().replacements, 0);
    p.play_track(track("current"));
    p.set_default_album(None);
    let snapshot = settle(&p);
    assert!(snapshot.playing);
    assert_eq!(snapshot.current.unwrap().id.item, "current");
}

#[test]
fn late_engine_snapshot_cannot_label_a_new_sequence_with_the_old_index() {
    let (service, fake) = setup();
    let p = service.playback();
    p.play_list(vec![track("old-one"), track("old-two")]);
    settle(&p);
    {
        let mut state = fake.lock().unwrap();
        state.clock.track = 1;
        state.clock.seconds = 87.0;
        state.hold_snapshot = true;
    }
    p.play_list(vec![track("new-one"), track("new-two")]);
    let snapshot = settle(&p);
    assert_eq!(snapshot.current.unwrap().id.item, "new-one");
    assert_eq!(snapshot.seconds, 0.0);
    {
        let mut state = fake.lock().unwrap();
        state.clock.sequence = state.next_sequence;
        state.clock.track = 0;
        state.clock.seconds = 3.0;
    }
    assert_eq!(settle(&p).seconds, 3.0);
}

#[test]
fn metadata_changes_names_without_replacing_media_access_or_playback_order() {
    let (service, fake) = setup();
    let p = service.playback();
    let original = track("original");
    p.play_track(original.clone());
    let mut updated = original.clone();
    updated.title = "Corrected title".into();
    updated.access = MediaAccess::File {
        locator: "/wrong.wav".into(),
    };
    p.update_metadata(Album {
        metadata: Default::default(),
        id: original.id.clone(),
        title: "Album".into(),
        artist: "Artist".into(),
        tracks: vec![updated],
        source_label: String::new(),
    });
    let current = settle(&p).current.unwrap();
    assert_eq!(current.title, "Corrected title");
    assert!(matches!(current.access, MediaAccess::File { locator } if locator == "/original.wav"));
    assert_eq!(fake.lock().unwrap().replacements, 1);
}

#[test]
fn engine_events_are_consumed_without_a_view_and_owner_shutdown_releases_engine() {
    let (service, fake) = setup();
    let p = service.playback();
    p.play_track(track("one"));
    settle(&p);
    fake.lock()
        .unwrap()
        .events
        .push_back(PlayerEvent::OutputUnavailable("route lost".into()));
    let notice = settle(&p).notice.unwrap();
    assert_eq!(notice.message, "route lost");
    fake.lock()
        .unwrap()
        .events
        .push_back(PlayerEvent::OutputRecovered);
    assert!(settle(&p).notice.unwrap().revision > notice.revision);
    drop(service);
    assert!(fake.lock().unwrap().dropped);
    assert!(!p.snapshot().playing);
    p.transport(Transport::Play); // retained native callback cannot resurrect a stopped owner
}

/// Hold the worker immediately after it publishes a clock, so the engine can
/// cross a boundary before the next command without timing-dependent sleeps.
fn hold_after_poll(p: &Playback) -> mpsc::Sender<()> {
    let (entered, waiting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(Some(gate));
    p.observe_now_playing(Some(Arc::new(move |_| {
        if let Some(gate) = gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    })));
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    release
}

#[test]
fn clear_queue_preserves_a_queued_track_that_started_after_the_last_poll() {
    let (service, fake) = setup();
    let p = service.playback();
    p.enqueue(track("queued-current"));
    p.enqueue(track("queued-next"));
    p.play_album(vec![track("one"), track("two")], 0);
    settle(&p);
    let release = hold_after_poll(&p);
    fake.lock().unwrap().clock.track = 2;
    p.clear_queue();
    release.send(()).unwrap();
    let state = settle(&p);
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["one", "two", "queued-current"]
    );
    assert_eq!(state.current.unwrap().id.item, "queued-current");
    assert!(state.playing);
    assert!(state.up_next.is_empty());
}

#[test]
fn play_track_does_not_repeat_a_queued_track_that_started_after_the_last_poll() {
    let (service, fake) = setup();
    let p = service.playback();
    p.enqueue(track("queued-current"));
    p.enqueue(track("queued-next"));
    p.play_album(vec![track("one"), track("two")], 0);
    settle(&p);
    let release = hold_after_poll(&p);
    fake.lock().unwrap().clock.track = 2;
    p.play_track(track("chosen"));
    release.send(()).unwrap();
    let state = settle(&p);
    assert_eq!(ids(&fake.lock().unwrap().tracks), ["chosen", "queued-next"]);
    assert_eq!(ids(&state.up_next), ["queued-next"]);
}

#[test]
fn queue_edits_follow_transport_commands_in_the_same_application_batch() {
    let (service, fake) = setup();
    let p = service.playback();
    p.enqueue(track("queued-current"));
    p.enqueue(track("queued-next"));
    p.play_album(vec![track("one"), track("two")], 0);
    settle(&p);
    let release = hold_after_poll(&p);
    p.transport(Transport::Next);
    p.transport(Transport::Next);
    p.clear_queue();
    release.send(()).unwrap();
    assert_eq!(settle(&p).current.unwrap().id.item, "queued-current");
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["one", "two", "queued-current"]
    );
}

#[test]
fn native_album_artwork_updates_and_follows_background_queue_transitions() {
    use std::collections::BTreeMap;
    let (service, fake) = setup();
    let p = service.playback();
    let received = Arc::new(Mutex::new(Vec::new()));
    let output = received.clone();
    p.observe_now_playing(Some(Arc::new(move |state| {
        output.lock().unwrap().push(state);
    })));
    let album = |name: &str, song: &str| Album {
        metadata: Default::default(),
        id: ItemId {
            library: "test".into(),
            provider: "test".into(),
            item: name.into(),
        },
        title: name.into(),
        artist: "Artist".into(),
        tracks: vec![track(song)],
        source_label: String::new(),
    };
    let library = Arc::new(Library {
        scan: None,
        id: "test".into(),
        albums: vec![album("First album", "one"), album("No cover", "two")],
        ..Default::default()
    });
    p.refresh_catalog_metadata(library.clone(), Arc::default());
    p.play_list(vec![track("one"), track("two")]);
    settle(&p);
    assert!(
        received
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .album
            .as_ref()
            .unwrap()
            .artwork_path
            .is_none()
    );
    // Artwork arriving for an already playing track must republish, even if
    // playback time and the track's own descriptive fields haven't changed.
    p.refresh_catalog_metadata(
        library,
        Arc::new(BTreeMap::from([(
            "First album".into(),
            "/cache/cover-without-extension".into(),
        )])),
    );
    settle(&p);
    {
        let received = received.lock().unwrap();
        let info = received.last().unwrap().album.as_ref().unwrap();
        assert_eq!(info.title, "First album");
        assert_eq!(
            info.artwork_path.as_deref(),
            Some("/cache/cover-without-extension")
        );
    }
    // Changing the browsing source must preserve metadata for the audible queue.
    p.refresh_catalog_metadata(Arc::new(Library::default()), Arc::default());
    settle(&p);
    assert_eq!(
        received
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .album
            .as_ref()
            .unwrap()
            .title,
        "First album"
    );
    fake.lock().unwrap().clock.track = 1;
    settle(&p);
    {
        let received = received.lock().unwrap();
        let state = received.last().unwrap();
        assert_eq!(state.track.as_ref().unwrap().id.item, "two");
        let info = state.album.as_ref().unwrap();
        assert_eq!(info.title, "No cover");
        assert!(info.artwork_path.is_none());
    }
    // Unknown tracks must not inherit the previous album's cover or title.
    p.play_track(track("unknown"));
    settle(&p);
    assert!(received.lock().unwrap().last().unwrap().album.is_none());
}

#[test]
fn lock_screen_seeks_keep_track_and_album_metadata_in_every_publication() {
    let (service, _) = setup();
    let p = service.playback();
    let received = Arc::new(Mutex::new(Vec::new()));
    let output = received.clone();
    p.refresh_catalog_metadata(
        Arc::new(Library {
            scan: None,
            albums: vec![Album {
                metadata: Default::default(),
                id: ItemId {
                    library: "test".into(),
                    provider: "test".into(),
                    item: "album".into(),
                },
                title: "Album".into(),
                artist: "Artist".into(),
                tracks: vec![track("one")],
                source_label: String::new(),
            }],
            ..Default::default()
        }),
        Arc::new(BTreeMap::from([("album".into(), "/cover".into())])),
    );
    p.play_track(track("one"));
    settle(&p);
    p.observe_now_playing(Some(Arc::new(move |state| {
        output.lock().unwrap().push(state)
    })));
    settle(&p);
    for playing in [true, false] {
        p.transport(if playing {
            Transport::Play
        } else {
            Transport::Pause
        });
        settle(&p);
        for seconds in [52.0, 8.0, 0.0, 99.0] {
            p.transport(Transport::Seek(seconds));
            let snapshot = settle(&p);
            assert_eq!(snapshot.current.unwrap().id.item, "one");
            assert_eq!(snapshot.seconds, seconds);
            let received = received.lock().unwrap();
            let last = received.last().unwrap();
            assert_eq!(last.seconds, seconds);
            assert_eq!(last.playing, playing);
        }
    }
    for state in received.lock().unwrap().iter() {
        assert_eq!(state.track.as_ref().unwrap().title, "one");
        let album = state.album.as_ref().unwrap();
        assert_eq!(album.title, "Album");
        assert_eq!(album.artwork_path.as_deref(), Some("/cover"));
    }
}

#[test]
fn saved_buffer_is_bounded_and_reaches_engine_before_playback() {
    for (requested, expected) in [(0, 3), (30, 30), (u32::MAX, 60)] {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let received = Arc::new(Mutex::new(None));
        let captured = received.clone();
        let service = PlaybackService::with_engine_factory(
            Arc::new(|| Ok(())),
            Box::new(move |_, seconds| {
                *captured.lock().unwrap() = Some(seconds);
                Ok(Box::new(Fake(state.clone())))
            }),
        )
        .unwrap();
        let playback = service.playback();
        playback.set_buffer_seconds(requested);
        playback.play_list(vec![track("buffer-test")]);
        settle(&playback);
        assert_eq!(*received.lock().unwrap(), Some(expected));
    }
}

#[test]
fn dropped_playlist_replaces_pending_tracks_and_ignores_catalog_name_updates() {
    let (service, fake) = setup();
    let p = service.playback();
    p.play_list(vec![track("old")]);
    p.enqueue(track("pending"));
    let mut dropped = track("local");
    dropped.id.library = "drop:session".into();
    dropped.title = "Local title".into();
    p.replace_queue(vec![dropped.clone(), track("second")]);
    let state = settle(&p);
    assert_eq!(state.current.unwrap().id, dropped.id);
    assert_eq!(ids(&state.up_next), ["second"]);
    assert!(state.playing);
    let mut catalog_track = dropped.clone();
    catalog_track.id.library = "catalog".into();
    catalog_track.title = "Catalog title".into();
    p.refresh_catalog_metadata(
        Arc::new(Library {
            scan: None,
            id: "catalog".into(),
            albums: vec![Album {
                metadata: Default::default(),
                id: catalog_track.id.clone(),
                title: "Album".into(),
                artist: "Artist".into(),
                tracks: vec![catalog_track],
                source_label: String::new(),
            }],
            ..Default::default()
        }),
        Arc::default(),
    );
    assert_eq!(settle(&p).current.unwrap().title, "Local title");
    assert_eq!(ids(&fake.lock().unwrap().tracks), ["local", "second"]);
}

#[test]
fn clearing_dropped_queue_keeps_current_track_and_removes_remainder() {
    let (service, fake) = setup();
    let p = service.playback();
    p.replace_queue(vec![track("first"), track("second"), track("third")]);
    assert_eq!(ids(&settle(&p).up_next), ["second", "third"]);
    p.transport(Transport::Next);
    assert_eq!(settle(&p).current.unwrap().id.item, "second");
    assert_eq!(ids(&settle(&p).up_next), ["third"]);
    p.clear_queue();
    let state = settle(&p);
    assert_eq!(state.current.unwrap().id.item, "second");
    assert!(state.up_next.is_empty());
    assert_eq!(ids(&fake.lock().unwrap().tracks), ["first", "second"]);
    // An empty replacement must preserve the current queue too.
    p.replace_queue(vec![]);
    assert_eq!(settle(&p).current.unwrap().id.item, "second");
}

#[test]
fn progressive_queue_appends_preserve_pause_and_navigation() {
    let (service, fake) = setup();
    let p = service.playback();
    let stop = Arc::new(AtomicBool::new(false));
    let mut first = track("first");
    first.id.library = "drop:stream".into();
    p.begin_file_discovery("drop:stream".into(), stop.clone());
    p.start_file_queue(first, stop.clone());
    assert_eq!(settle(&p).current.unwrap().id.item, "first");
    p.transport(Transport::Pause);
    p.transport(Transport::Seek(23.));
    settle(&p);
    p.append_file_queue("drop:stream".into(), vec![track("second"), track("third")]);
    let state = settle(&p);
    assert!(!state.playing);
    assert_eq!(state.seconds, 23.);
    assert_eq!(ids(&state.up_next), ["second", "third"]);
    p.transport(Transport::Next);
    assert_eq!(settle(&p).current.unwrap().id.item, "second");
    assert!(!settle(&p).playing);
    p.finish_file_queue("drop:stream".into());
    assert_eq!(ids(&settle(&p).up_next), ["third"]);
    assert!(
        !stop.load(Ordering::Relaxed),
        "Finishing must not invalidate accepted commands"
    );
    assert_eq!(fake.lock().unwrap().replacements, 1);
}
#[test]
fn cancelling_discovery_preserves_accepted_tracks_and_rejects_late_commands() {
    let (service, _) = setup();
    let p = service.playback();
    let stop = Arc::new(AtomicBool::new(false));
    let first = track("first");
    p.start_file_queue(first, stop.clone());
    settle(&p);
    p.append_file_queue("test".into(), vec![track("second")]);
    settle(&p);
    stop.store(true, Ordering::Relaxed);
    p.append_file_queue("test".into(), vec![track("late")]);
    assert_eq!(ids(&settle(&p).up_next), ["second"]);
    let cancelled = Arc::new(AtomicBool::new(true));
    p.start_file_queue(track("never"), cancelled);
    assert_eq!(settle(&p).current.unwrap().id.item, "first");
}
#[test]
fn replacing_or_clearing_a_progressive_queue_cancels_discovery() {
    let (service, _) = setup();
    let p = service.playback();
    for clear in [false, true] {
        let stop = Arc::new(AtomicBool::new(false));
        p.start_file_queue(track("first"), stop.clone());
        settle(&p);
        if clear {
            p.clear_queue();
        } else {
            p.play_album(vec![track("replacement")], 0);
        }
        settle(&p);
        assert!(stop.load(Ordering::Relaxed));
        p.append_file_queue("test".into(), vec![track("late")]);
        assert!(settle(&p).up_next.is_empty());
        assert_eq!(
            settle(&p).current.unwrap().id.item,
            if clear { "first" } else { "replacement" }
        );
    }
}

#[test]
fn choosing_music_before_first_discovery_prevents_a_late_start() {
    let (service, _) = setup();
    let p = service.playback();
    let stop = Arc::new(AtomicBool::new(false));
    p.begin_file_discovery("test".into(), stop.clone());
    p.play_album(vec![track("chosen")], 0);
    p.start_file_queue(track("late-first"), stop.clone());
    assert_eq!(settle(&p).current.unwrap().id.item, "chosen");
    assert!(stop.load(Ordering::Relaxed));
}

#[test]
fn blocked_tracks_are_filtered_without_changing_requested_start_or_explicit_queue() {
    let (service, fake) = setup();
    let playback = service.playback();
    let mut blocked = track("protected");
    blocked.blocked_reason = Some(amp_core::PlaybackBlockReason::Protected);
    playback.enqueue(track("explicit"));
    playback.play_album(vec![blocked.clone(), track("one"), track("two")], 2);
    settle(&playback);
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["one", "two", "explicit"]
    );
    assert_eq!(fake.lock().unwrap().clock.track, 1);
    playback.play_track(blocked.clone());
    playback.enqueue(blocked.clone());
    let snapshot = settle(&playback);
    assert!(snapshot.notice.unwrap().message.contains("Protected"));
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["one", "two", "explicit"]
    );
    playback.play_list_from(vec![blocked.clone(), track("must-not-start")]);
    settle(&playback);
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["one", "two", "explicit"]
    );
    playback.play_list(vec![blocked]);
    settle(&playback);
    assert_eq!(
        ids(&fake.lock().unwrap().tracks),
        ["one", "two", "explicit"]
    );
}

#[test]
fn album_and_idle_play_choose_first_playable_track() {
    let (service, fake) = setup();
    let playback = service.playback();
    let mut blocked = track("cloud");
    blocked.blocked_reason = Some(amp_core::PlaybackBlockReason::NotLocal);
    playback.play_album_start(vec![blocked.clone(), track("local")]);
    settle(&playback);
    assert_eq!(ids(&fake.lock().unwrap().tracks), ["local"]);
    drop(service);
    let (service, fake) = setup();
    let playback = service.playback();
    playback.set_default_album(Some((vec![blocked, track("another")], false)));
    playback.transport(Transport::Play);
    settle(&playback);
    assert_eq!(ids(&fake.lock().unwrap().tracks), ["another"]);
}

#[test]
fn access_revocation_uses_audible_track_and_preserves_other_providers() {
    let (service, fake) = setup();
    let playback = service.playback();
    let mut native = track("native");
    native.id.provider = "native".into();
    playback.play_album_start(vec![native.clone(), track("current"), native.clone()]);
    settle(&playback);
    {
        let mut state = fake.lock().unwrap();
        state.clock.track = 1;
        state.clock.seconds = 23.0;
    }
    playback.revoke_provider("native".into());
    let snapshot = settle(&playback);
    assert_eq!(snapshot.current.unwrap().title, "current");
    assert_eq!(snapshot.seconds, 23.0);
    assert!(snapshot.playing);
    assert_eq!(ids(&fake.lock().unwrap().tracks), ["native", "current"]);
    playback.play_track(native);
    settle(&playback);
    playback.enqueue(track("retained"));
    settle(&playback);
    playback.revoke_provider("native".into());
    let snapshot = settle(&playback);
    assert!(snapshot.current.is_none());
    assert!(!snapshot.playing);
    assert_eq!(ids(&snapshot.up_next), ["retained"]);
}

#[test]
fn idle_selection_receives_availability_updates_without_a_ui_tick() {
    let (service, fake) = setup();
    let playback = service.playback();
    let original = track("idle");
    playback.set_default_album(Some((vec![original.clone()], false)));
    settle(&playback);
    let mut updated = original;
    updated.blocked_reason = Some(amp_core::PlaybackBlockReason::Protected);
    let library = Library {
        scan: None,
        id: "test".into(),
        albums: vec![Album {
            metadata: Default::default(),
            id: ItemId {
                library: "test".into(),
                provider: "test".into(),
                item: "album".into(),
            },
            title: "Album".into(),
            artist: "Artist".into(),
            tracks: vec![updated],
            source_label: "Test".into(),
        }],
        ..Default::default()
    };
    playback.refresh_catalog_metadata(Arc::new(library), Arc::default());
    playback.transport(Transport::Play);
    let snapshot = settle(&playback);
    assert!(!snapshot.playing);
    assert!(snapshot.notice.unwrap().message.contains("No playable"));
    assert_eq!(fake.lock().unwrap().replacements, 0);
}

#[test]
fn device_library_play_actions_cancel_progressive_file_discovery() {
    let (service, _) = setup();
    let playback = service.playback();
    for album_start in [true, false] {
        let stop = Arc::new(AtomicBool::new(false));
        playback.start_file_queue(track("dropped"), stop.clone());
        settle(&playback);
        if album_start {
            playback.play_album_start(vec![track("chosen")]);
        } else {
            playback.play_list_from(vec![track("chosen")]);
        }
        assert_eq!(settle(&playback).current.unwrap().id.item, "chosen");
        assert!(stop.load(Ordering::Relaxed));
        playback.append_file_queue("test".into(), vec![track("late-drop")]);
        assert!(settle(&playback).up_next.is_empty());
    }
}

#[test]
fn processing_is_installed_before_first_play_and_updates_without_replacing_audio() {
    let (service, fake) = setup();
    let playback = service.playback();
    let settings = AudioProcessingSettings {
        crossfeed: amp_core::CrossfeedSettings {
            enabled: true,
            cutoff_hz: 650,
            feed_db_tenths: 95,
        },
    };
    playback.set_processing(settings);
    playback.play_track(track("a"));
    settle(&playback);
    assert_eq!(fake.lock().unwrap().processing_at_play, settings);
    let before = playback.snapshot();
    let updated = AudioProcessingSettings {
        crossfeed: amp_core::CrossfeedSettings {
            enabled: false,
            cutoff_hz: 0,
            feed_db_tenths: 999,
        },
    };
    playback.set_processing(updated);
    settle(&playback);
    let state = fake.lock().unwrap();
    assert_eq!(state.processing, updated.normalized());
    assert_eq!(state.replacements, 1);
    assert_eq!(playback.snapshot().seconds, before.seconds);
    assert_eq!(playback.snapshot().playing, before.playing);
}

#[test]
fn skipped_track_failures_survive_progress_and_ignore_stale_sequences() {
    let (service, fake) = setup();
    let playback = service.playback();
    playback.replace_queue(vec![track("first"), track("missing"), track("last")]);
    settle(&playback);
    let sequence = fake.lock().unwrap().clock.sequence;
    fake.lock()
        .unwrap()
        .events
        .push_back(PlayerEvent::TrackUnplayable {
            sequence,
            index: 1,
            reason: "File not found".into(),
        });
    fake.lock().unwrap().clock.track = 2;
    let snapshot = settle(&playback);
    assert_eq!(snapshot.current.unwrap().id.item, "last");
    assert_eq!(snapshot.unplayable.len(), 1);
    assert_eq!(snapshot.unplayable[0].track.id.item, "missing");
    assert_eq!(snapshot.unplayable[0].reason, "File not found");
    playback.replace_queue(vec![track("replacement")]);
    settle(&playback);
    fake.lock()
        .unwrap()
        .events
        .push_back(PlayerEvent::TrackUnplayable {
            sequence,
            index: 0,
            reason: "Old failure".into(),
        });
    assert!(settle(&playback).unplayable.is_empty());
}

#[test]
fn pause_gates_rendering_while_app_worker_waits_for_a_queue_edit() {
    let (service, fake) = setup();
    let playback = service.playback();
    playback.play_track(track("playing"));
    settle(&playback);
    let (entered, waiting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let leases = {
        let mut state = fake.lock().unwrap();
        state.edit_gate = Some((entered, gate));
        state.pause_leases.clone()
    };
    playback.clear_queue();
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    playback.transport(Transport::Play); // An earlier command must not undo the gate.
    playback.transport(Transport::Pause);
    assert_eq!(leases.load(Ordering::Acquire), 1);
    release.send(()).unwrap();
    assert!(!settle(&playback).playing);
    assert_eq!(leases.load(Ordering::Acquire), 0);
    assert_eq!(fake.lock().unwrap().resume_gate_counts, [1]);
}
