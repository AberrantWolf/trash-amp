//! Exercise the real worker/decoder/callback path with controllable device lifetimes.
use super::*;
use output::{Backend, Health, Output};
use std::{
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Device {
    key: String,
    rate: u32,
    fails: bool,
}
impl Device {
    fn new(key: &str, rate: u32) -> Self {
        Self {
            key: key.into(),
            rate,
            fails: false,
        }
    }
}

struct Session {
    shared: Arc<Shared>,
    consumer: Mutex<rtrb::Consumer<Frame>>,
    health: Arc<Health>,
    alive: AtomicBool,
}
impl Session {
    fn wait_frames(&self, count: usize) {
        wait(|| self.consumer.lock().unwrap().slots() >= count);
    }
    fn render(&self, count: usize) -> Vec<f32> {
        let mut samples = vec![0.0; count * 2];
        output::render(
            &mut samples,
            2,
            &mut self.consumer.lock().unwrap(),
            &self.shared,
            &self.health,
        );
        samples
    }
}
struct Stream(Arc<Session>);
impl Drop for Stream {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct Devices {
    current: Option<Device>,
    sessions: Vec<Arc<Session>>,
    attempts: Vec<Instant>,
}
struct FakeBackend {
    devices: Arc<Mutex<Devices>>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}
impl Backend for FakeBackend {
    type Stream = Stream;
    fn default_key(&mut self) -> Result<String> {
        self.devices
            .lock()
            .unwrap()
            .current
            .as_ref()
            .map(|device| device.key.clone())
            .context("No default output")
    }
    fn open(&mut self, shared: Arc<Shared>) -> Result<Output<Self::Stream>> {
        self.devices.lock().unwrap().attempts.push(Instant::now());
        if let Some(gate) = &self.gate {
            let (lock, changed) = &**gate;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = changed.wait(ready).unwrap();
            }
        }
        let mut devices = self.devices.lock().unwrap();
        let device = devices.current.clone().context("No default output")?;
        if device.fails {
            bail!("Output is temporarily unavailable");
        }
        let (producer, consumer) = rtrb::RingBuffer::new(8_192);
        let health = Arc::new(Health::default());
        let session = Arc::new(Session {
            shared,
            consumer: Mutex::new(consumer),
            health: health.clone(),
            alive: AtomicBool::new(true),
        });
        devices.sessions.push(session.clone());
        Ok(Output {
            _stream: Stream(session),
            producer,
            rate: device.rate,
            key: device.key,
            health,
        })
    }
}

struct Harness {
    player: Player,
    devices: Arc<Mutex<Devices>>,
    thread: Option<thread::JoinHandle<()>>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}
impl Harness {
    fn new(current: Option<Device>) -> Self {
        Self::with_gate(current, None)
    }
    fn with_gate(current: Option<Device>, gate: Option<Arc<(Mutex<bool>, Condvar)>>) -> Self {
        let devices = Arc::new(Mutex::new(Devices {
            current,
            ..Default::default()
        }));
        let (player, thread) = Player::spawn(
            None,
            FakeBackend {
                devices: devices.clone(),
                gate: gate.clone(),
            },
        )
        .unwrap();
        Self {
            player,
            devices,
            thread: Some(thread),
            gate,
        }
    }
    fn session(&self, index: usize) -> Arc<Session> {
        wait(|| self.devices.lock().unwrap().sessions.len() > index);
        wait(|| self.player.snapshot().output == OutputState::Ready);
        self.devices.lock().unwrap().sessions[index].clone()
    }
    fn device(&self, device: Option<Device>) {
        self.devices.lock().unwrap().current = device;
    }
    fn disconnect(&self, session: &Session) {
        self.device(None);
        session.health.failed.store(true, Ordering::Release);
        wait(|| self.player.snapshot().output == OutputState::Unavailable);
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.player.shared.shutdown.store(true, Ordering::Relaxed);
        if let Some(gate) = &self.gate {
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        }
        self.thread.take().unwrap().join().unwrap();
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !condition() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(condition(), "audio worker did not reach the expected state");
}
fn playing(player: &Player, expected: bool) {
    wait(|| player.snapshot().playing == expected);
}

#[test]
fn worker_joins_tracks_without_dropping_or_repeating_frames() {
    let (_dir, track) = tests::fixture(44_100, 1_176);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let session = harness.session(0);
    harness.player.play(vec![track.clone(), track], 0);
    session.wait_frames(2_352);
    let samples = session.render(2_352);
    for (index, frame) in samples.chunks_exact(2).enumerate() {
        assert_eq!(frame[0], ((index % 1_176) % 1_000) as f32 / 32_768.0);
        assert_eq!(frame[1], -frame[0]);
    }
    assert_eq!(harness.player.snapshot().track, 1);
    assert_eq!(
        harness.player.shared.position.load(Ordering::Relaxed),
        1_176
    );
    assert!(harness.player.event().is_none());
}

#[test]
fn disconnect_preserves_audible_position_queue_and_pauses_until_play() {
    let (_dir, track) = tests::fixture(44_100, 1_176);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let old = harness.session(0);
    harness.player.play(vec![track.clone(), track], 0);
    old.wait_frames(2_352); // The decoder has already read ahead into the next track.
    old.render(37);
    harness.disconnect(&old);
    assert!(!harness.player.snapshot().playing);
    assert_eq!(harness.player.snapshot().level, 0.0);
    assert!(!old.alive.load(Ordering::Acquire));
    assert_eq!(harness.player.shared.position.load(Ordering::Relaxed), 37);
    harness.device(Some(Device::new("replacement", 44_100)));
    let new = harness.session(1);
    assert!(!harness.player.snapshot().playing);
    assert_eq!(new.render(10), vec![0.0; 20]);
    assert_eq!(harness.player.shared.position.load(Ordering::Relaxed), 37);
    harness.player.set_playing(true);
    new.wait_frames(2_352 - 37);
    let samples = new.render(2_352 - 37);
    for (index, frame) in samples.chunks_exact(2).enumerate() {
        assert_eq!(frame[0], (((index + 37) % 1_176) % 1_000) as f32 / 32_768.0);
    }
    assert_eq!(harness.player.snapshot().track, 1);
    assert!(matches!(
        harness.player.event(),
        Some(PlayerEvent::OutputUnavailable(_))
    ));
    assert!(matches!(
        harness.player.event(),
        Some(PlayerEvent::OutputRecovered)
    ));
    assert!(harness.player.event().is_none());
}

#[test]
fn changed_sample_rate_preserves_time_and_future_seeks_use_the_new_rate() {
    let (_dir, track) = tests::fixture(44_100, 88_200);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let old = harness.session(0);
    harness.player.play(vec![track.clone()], 0);
    old.wait_frames(4_410);
    old.render(4_410);
    harness.disconnect(&old);
    harness.device(Some(Device::new("48khz", 48_000)));
    let new = harness.session(1);
    assert_eq!(
        harness.player.shared.position.load(Ordering::Relaxed),
        4_800
    );
    assert_eq!(harness.player.snapshot().seconds, 0.1);
    harness.player.seek(0.25);
    wait(|| harness.player.shared.position.load(Ordering::Relaxed) == 12_000);
    harness.player.set_playing(true);
    new.wait_frames(32);
    let samples = new.render(32);
    let mut reference = Converted::new(&track, 12_000, 48_000, None).unwrap();
    for frame in samples.chunks_exact(2) {
        assert_eq!(frame, reference.next().unwrap().unwrap());
    }
}

#[test]
fn transport_and_queue_edits_survive_absent_output_and_pause_cancels_pending_play() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let harness = Harness::new(None);
    wait(|| harness.player.snapshot().output == OutputState::Unavailable);
    harness.player.play(vec![track.clone(), track.clone()], 0);
    harness.player.set_playing(false);
    harness.player.seek(0.01);
    harness.player.append(vec![track.clone()]);
    harness.player.next();
    harness.player.seek(0.02);
    harness.player.previous();
    harness.player.truncate(1);
    harness.player.append(vec![track]);
    harness.player.next();
    harness.player.seek(0.003);
    harness.player.set_playing(true);
    wait(|| {
        harness.player.snapshot().track == 1
            && harness.player.shared.position.load(Ordering::Relaxed) == 132
    });
    harness.player.set_playing(false);
    playing(&harness.player, false);
    harness.device(Some(Device::new("restored", 44_100)));
    let session = harness.session(0);
    assert!(!harness.player.snapshot().playing);
    harness.player.set_playing(true);
    session.wait_frames(10);
    assert_eq!(session.render(10)[0], 132.0 / 32_768.0);
    assert_eq!(harness.player.snapshot().track, 1);
}

#[test]
fn failed_opens_are_rate_limited_and_play_requested_while_disconnected_is_preserved() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let mut device = Device::new("first", 44_100);
    device.fails = true;
    let harness = Harness::new(Some(device.clone()));
    harness.player.play(vec![track], 0);
    harness.player.seek(0.01);
    harness.player.set_playing(true);
    wait(|| harness.devices.lock().unwrap().attempts.len() >= 2);
    assert_eq!(harness.player.snapshot().seconds, 0.01);
    let times = harness.devices.lock().unwrap().attempts.clone();
    assert!(
        times
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0]) >= Duration::from_millis(950))
    );
    device.fails = false;
    harness.device(Some(device));
    let session = harness.session(0);
    session.wait_frames(10);
    assert_eq!(session.render(10)[0], 441.0 / 32_768.0);
    assert!(harness.player.snapshot().playing);
    assert!(matches!(
        harness.player.event(),
        Some(PlayerEvent::OutputUnavailable(_))
    ));
    assert!(matches!(
        harness.player.event(),
        Some(PlayerEvent::OutputRecovered)
    ));
    assert!(harness.player.event().is_none());
}

#[test]
fn default_route_change_pauses_and_stale_errors_cannot_stop_the_new_stream() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let old = harness.session(0);
    harness.player.play(vec![track], 0);
    old.wait_frames(10);
    old.render(10);
    harness.device(Some(Device::new("new-default", 44_100))); // No error callback.
    let new = harness.session(1);
    assert!(!harness.player.snapshot().playing);
    assert_eq!(harness.player.shared.position.load(Ordering::Relaxed), 10);
    harness.player.set_playing(true);
    new.wait_frames(20);
    old.health.failed.store(true, Ordering::Release);
    assert_eq!(old.render(20), vec![0.0; 40]);
    assert_eq!(new.render(20)[0], 10.0 / 32_768.0);
    assert!(harness.player.snapshot().playing);
    assert_eq!(harness.devices.lock().unwrap().sessions.len(), 2);
}

#[test]
fn streams_that_fail_immediately_after_open_are_rate_limited() {
    let harness = Harness::new(Some(Device::new("unstable", 44_100)));
    for index in 0..3 {
        let session = harness.session(index);
        session.health.failed.store(true, Ordering::Release);
    }
    let times = harness.devices.lock().unwrap().attempts.clone();
    assert_eq!(times.len(), 3);
    assert!(
        times
            .windows(2)
            .all(|pair| { pair[1].duration_since(pair[0]) >= Duration::from_millis(950) })
    );
}

#[test]
fn clearing_the_queue_while_disconnected_never_replays_buffered_audio() {
    let (_dir, track) = tests::fixture(44_100, 1_000);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let old = harness.session(0);
    let initial = harness.player.play(vec![track.clone()], 0);
    wait(|| harness.player.snapshot().sequence == initial);
    old.wait_frames(1_000);
    old.render(20);
    harness.device(None);
    old.health.failed.store(true, Ordering::Release);
    wait(|| harness.player.snapshot().output == OutputState::Unavailable);
    let cleared = harness.player.play(vec![], 0);
    assert!(cleared > initial);
    wait(|| {
        let snapshot = harness.player.snapshot();
        snapshot.sequence == cleared && snapshot.seconds == 0.0
    });
    harness.device(Some(Device::new("second", 44_100)));
    let new = harness.session(1);
    assert!(!harness.player.snapshot().playing);
    assert_eq!(new.render(100), vec![0.0; 200]);
    harness.player.play(vec![track], 0);
    new.wait_frames(100);
    assert_eq!(new.render(100)[2], 1.0 / 32_768.0);
}

#[test]
fn a_stalled_callback_is_reopened_without_automatically_resuming() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let _old = harness.session(0);
    harness.player.play(vec![track], 0);
    let _new = harness.session(1); // No callbacks: exercises the real watchdog timeout.
    assert!(!harness.player.snapshot().playing);
    assert_eq!(harness.player.snapshot().seconds, 0.0);
}

#[test]
fn opening_a_device_never_blocks_the_callers_transport() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let start = Instant::now();
    let harness = Harness::with_gate(Some(Device::new("first", 44_100)), Some(gate.clone()));
    let (_dir, track) = tests::fixture(44_100, 44_100);
    harness.player.play(vec![track], 0);
    harness.player.seek(0.2);
    harness.player.set_playing(false);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(harness.player.snapshot().output, OutputState::Connecting);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let _output = harness.session(0);
    wait(|| harness.player.snapshot().seconds == 0.2);
    assert!(!harness.player.snapshot().playing);
}

#[test]
fn dropping_the_player_does_not_wait_for_a_blocked_device_open() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let devices = Arc::new(Mutex::new(Devices {
        current: Some(Device::new("blocked", 44_100)),
        ..Default::default()
    }));
    let (player, worker) = Player::spawn(
        None,
        FakeBackend {
            devices: devices.clone(),
            gate: Some(gate.clone()),
        },
    )
    .unwrap();
    wait(|| !devices.lock().unwrap().attempts.is_empty());
    let start = Instant::now();
    drop(player);
    let elapsed = start.elapsed();
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    worker.join().unwrap();
    assert!(elapsed < Duration::from_secs(1));
    assert!(
        devices
            .lock()
            .unwrap()
            .sessions
            .iter()
            .all(|session| !session.alive.load(Ordering::Acquire))
    );
}

#[test]
fn queue_edit_uses_audible_position_after_pending_transport_commands() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let harness = Harness::new(None);
    let tracks = vec![track.clone(); 4];
    harness.player.play(tracks.clone(), 0);
    harness.player.next();
    harness.player.next();
    let update = harness
        .player
        .edit_queue(Box::new(move |index| {
            assert_eq!(index, 2, "edit must follow both Next commands");
            let mut tracks = tracks;
            tracks.truncate(index + 1);
            (tracks, None)
        }))
        .unwrap();
    assert_eq!(update.tracks.len(), 3);
    assert_eq!(update.clock.track, 2);
    assert!(update.clock.playing);
    assert_eq!(update.clock.sequence, 2);
}

#[test]
fn render_never_advances_the_audible_clock_during_a_queue_edit() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let mut harness = Harness::new(Some(Device::new("first", 44_100)));
    let session = harness.session(0);
    harness.player.play(vec![track.clone(); 4], 0);
    session.wait_frames(1_000);
    session.render(37);
    let before = harness.player.snapshot();
    let (entered, waiting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    thread::scope(|scope| {
        let player = &mut harness.player;
        let edit = scope.spawn(move || {
            player.edit_queue(Box::new(move |index| {
                entered.send(index).unwrap();
                gate.recv_timeout(Duration::from_secs(2)).unwrap();
                (vec![track; index + 1], None)
            }))
        });
        let index = waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        let samples = session.render(1_000);
        let during_track = session.shared.track.load(Ordering::Relaxed);
        let during_seconds = session.shared.position.load(Ordering::Relaxed) as f64
            / f64::from(session.shared.sample_rate.load(Ordering::Acquire));
        release.send(()).unwrap();
        let update = edit.join().unwrap().unwrap();
        assert_eq!(index, before.track);
        assert!(samples.iter().all(|sample| *sample == 0.0));
        assert_eq!(during_track, before.track);
        assert_eq!(during_seconds, before.seconds);
        assert_eq!(update.clock.seconds, before.seconds);
    });
}
