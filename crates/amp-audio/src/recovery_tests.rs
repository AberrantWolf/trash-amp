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
    processing: Mutex<processing::ProcessingChain>,
    alive: AtomicBool,
    running: AtomicBool,
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
            &mut self.processing.lock().unwrap(),
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
    checks: usize,
}
struct FakeBackend {
    devices: Arc<Mutex<Devices>>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    release_when_paused: bool,
}
impl Backend for FakeBackend {
    type Stream = Stream;
    fn release_when_paused(&self) -> bool {
        self.release_when_paused
    }
    fn set_playing(&mut self, stream: &mut Stream, playing: bool) -> Result<()> {
        stream.0.running.store(playing, Ordering::Release);
        Ok(())
    }
    fn default_key(&mut self) -> Result<String> {
        let mut devices = self.devices.lock().unwrap();
        devices.checks += 1;
        devices
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
        let (producer, consumer) = rtrb::RingBuffer::new(if self.release_when_paused {
            output::buffer_capacity(device.rate, true)
        } else {
            8_192
        });
        let health = Arc::new(Health::default());
        let session = Arc::new(Session {
            processing: Mutex::new(processing::ProcessingChain::new(device.rate, 2)),
            shared,
            consumer: Mutex::new(consumer),
            health: health.clone(),
            alive: AtomicBool::new(true),
            running: AtomicBool::new(false),
        });
        devices.sessions.push(session.clone());
        Ok(Output {
            stream: Stream(session),
            running: false,
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
        Self::with_policy(current, gate, false)
    }
    fn with_policy(
        current: Option<Device>,
        gate: Option<Arc<(Mutex<bool>, Condvar)>>,
        release_when_paused: bool,
    ) -> Self {
        Self::with_resolver(current, gate, release_when_paused, None)
    }
    fn with_resolver(
        current: Option<Device>,
        gate: Option<Arc<(Mutex<bool>, Condvar)>>,
        release_when_paused: bool,
        resolver: Option<Arc<dyn MediaResolver>>,
    ) -> Self {
        let devices = Arc::new(Mutex::new(Devices {
            current,
            ..Default::default()
        }));
        let (player, thread) = Player::spawn(
            resolver,
            FakeBackend {
                devices: devices.clone(),
                gate: gate.clone(),
                release_when_paused,
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
        let _ = self.player.commands.send(Command::Shutdown);
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
fn pause_stops_output_without_watchdog_reopens_and_resumes_exact_buffered_audio() {
    let (_dir, track) = tests::fixture(44_100, 1_176);
    let harness = Harness::new(Some(Device::new("first", 44_100)));
    let session = harness.session(0);
    harness.player.play(vec![track.clone(), track], 0);
    session.wait_frames(2_352);
    session.render(37);
    harness.player.set_playing(false);
    wait(|| !session.running.load(Ordering::Acquire));
    assert_eq!(harness.player.snapshot().level, 0.0);
    let checks = harness.devices.lock().unwrap().checks;
    // Exceed the callback watchdog without supplying any callbacks.
    thread::sleep(Duration::from_millis(3_200));
    assert_eq!(harness.devices.lock().unwrap().checks, checks);
    assert_eq!(harness.devices.lock().unwrap().sessions.len(), 1);
    assert_eq!(harness.player.shared.position.load(Ordering::Relaxed), 37);
    assert!(!session.running.load(Ordering::Acquire));
    assert!(harness.player.event().is_none());
    harness.player.set_playing(true);
    wait(|| session.running.load(Ordering::Acquire));
    for (index, frame) in session.render(2_352 - 37).chunks_exact(2).enumerate() {
        assert_eq!(frame[0], (((index + 37) % 1_176) % 1_000) as f32 / 32_768.0);
        assert_eq!(frame[1], -frame[0]);
    }
    // Natural queue completion must release the device as well as manual pause.
    wait(|| !session.running.load(Ordering::Acquire));
    assert!(!harness.player.snapshot().playing);
    assert_eq!(harness.player.snapshot().level, 0.0);
}

#[test]
fn paused_seek_and_queue_changes_resume_at_the_requested_frame() {
    for release_when_paused in [false, true] {
        let (_dir, track) = tests::fixture(44_100, 44_100);
        let harness = Harness::with_policy(
            Some(Device::new("first", 44_100)),
            None,
            release_when_paused,
        );
        harness.player.play(vec![track.clone(); 2], 0);
        let session = harness.session(0);
        session.wait_frames(100);
        session.render(37);
        harness.player.toggle();
        wait(|| !session.running.load(Ordering::Acquire));
        harness.player.next();
        harness.player.seek(0.01);
        // A synchronous edit fences all preceding transport commands.
        let update = harness
            .player
            .edit_queue(Box::new(move |index| {
                assert_eq!(index, 1);
                (vec![track; 2], None)
            }))
            .unwrap();
        assert!(!update.clock.playing);
        assert_eq!(update.clock.seconds, 0.01);
        assert!(!session.running.load(Ordering::Acquire));
        harness.player.toggle();
        let resumed = if release_when_paused {
            harness.session(1)
        } else {
            session
        };
        wait(|| resumed.running.load(Ordering::Acquire));
        resumed.render(0); // Retire pre-seek frames as the first resumed callback does.
        wait(|| {
            resumed.consumer.lock().unwrap().peek().is_ok_and(|frame| {
                frame.generation == harness.player.shared.generation.load(Ordering::Acquire)
            })
        });
        assert_eq!(resumed.render(10)[0], 441.0 / 32_768.0);
    }
}

#[test]
fn mobile_pause_releases_output_without_reopening_until_play_and_restores_audible_position() {
    let (_dir, track) = tests::fixture(44_100, 1_176);
    let harness = Harness::with_policy(Some(Device::new("first", 44_100)), None, true);
    harness.player.play(vec![track.clone(), track], 0);
    let old = harness.session(0);
    old.wait_frames(2_352);
    old.render(37); // The decoder is already in the next track.
    harness.player.set_playing(false);
    wait(|| !old.alive.load(Ordering::Acquire));
    let attempts = harness.devices.lock().unwrap().attempts.len();
    thread::sleep(Duration::from_millis(1_200));
    assert_eq!(harness.devices.lock().unwrap().attempts.len(), attempts);
    assert_eq!(harness.player.shared.position.load(Ordering::Relaxed), 37);
    assert_eq!(harness.player.snapshot().level, 0.0);
    assert!(!harness.player.snapshot().playing);
    assert!(harness.player.event().is_none());
    harness.player.set_playing(true);
    let new = harness.session(1);
    new.wait_frames(2_352 - 37);
    for (index, frame) in new.render(2_352 - 37).chunks_exact(2).enumerate() {
        assert_eq!(frame[0], (((index + 37) % 1_176) % 1_000) as f32 / 32_768.0);
    }
    wait(|| !new.alive.load(Ordering::Acquire));
    assert!(!harness.player.snapshot().playing);
    assert!(harness.player.event().is_none());
}

#[derive(Default)]
struct NetworkGate {
    blocked: Mutex<bool>,
    changed: Condvar,
    entered: AtomicBool,
    reads: AtomicUsize,
}
impl NetworkGate {
    fn release(&self) {
        *self.blocked.lock().unwrap() = false;
        self.changed.notify_all();
    }
}
struct NetworkReader {
    bytes: std::io::Cursor<Vec<u8>>,
    gate: Arc<NetworkGate>,
}
impl std::io::Read for NetworkReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let mut blocked = self.gate.blocked.lock().unwrap();
        while *blocked {
            self.gate.entered.store(true, Ordering::Release);
            let (guard, timeout) = self
                .gate
                .changed
                .wait_timeout(blocked, Duration::from_secs(3))
                .unwrap();
            blocked = guard;
            if timeout.timed_out() && *blocked {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
        }
        self.gate.reads.fetch_add(1, Ordering::Relaxed);
        std::io::Read::read(&mut self.bytes, bytes)
    }
}
impl std::io::Seek for NetworkReader {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        std::io::Seek::seek(&mut self.bytes, from)
    }
}
struct NetworkResolver {
    bytes: Vec<u8>,
    gate: Arc<NetworkGate>,
    opens: AtomicUsize,
}
impl MediaResolver for NetworkResolver {
    fn resolve(&self, _: &Track) -> Result<ByteSource> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(ByteSource::new(
            NetworkReader {
                bytes: std::io::Cursor::new(self.bytes.clone()),
                gate: self.gate.clone(),
            },
            MediaHint::new(
                Some(self.bytes.len() as u64),
                Some("wav"),
                Some("audio/wav"),
            ),
        ))
    }
    fn cancel(&self) {
        self.gate.release();
    }
}

#[test]
fn mobile_resume_plays_retained_pcm_while_network_refill_is_blocked() {
    let (_dir, mut track) = tests::fixture(44_100, 44_100 * 40);
    let MediaAccess::File { locator } = &track.access else {
        unreachable!()
    };
    let gate = Arc::new(NetworkGate::default());
    let resolver = Arc::new(NetworkResolver {
        bytes: std::fs::read(locator).unwrap(),
        gate: gate.clone(),
        opens: AtomicUsize::new(0),
    });
    track.access = MediaAccess::Managed {
        provider: "network-test".into(),
        item: "track".into(),
    };
    let harness = Harness::with_resolver(
        Some(Device::new("first", 44_100)),
        None,
        true,
        Some(resolver.clone()),
    );
    harness.player.play(vec![track], 0);
    let old = harness.session(0);
    let capacity = output::buffer_capacity(44_100, true);
    old.wait_frames(capacity);
    old.render(147);
    harness.player.set_playing(false);
    wait(|| !old.alive.load(Ordering::Acquire));
    *gate.blocked.lock().unwrap() = true;
    let reads = gate.reads.load(Ordering::Relaxed);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(gate.reads.load(Ordering::Relaxed), reads);
    assert!(
        !gate.entered.load(Ordering::Acquire),
        "pause must not fetch more data"
    );
    harness.player.set_playing(true);
    let new = harness.session(1);
    wait(|| new.running.load(Ordering::Acquire));
    assert!(new.consumer.lock().unwrap().slots() >= capacity - 147);
    assert_eq!(
        resolver.opens.load(Ordering::Relaxed),
        1,
        "resume must reuse the dormant reader"
    );
    let mut position = 147;
    // Make the decoder reach its next network read, leaving at least two
    // seconds of PCM available to cover that read's latency.
    while !gate.entered.load(Ordering::Acquire) && position < 44_100 {
        assert_frames(&new.render(4_410), position);
        position += 4_410;
        thread::sleep(Duration::from_millis(5));
    }
    assert!(gate.entered.load(Ordering::Acquire));
    // A five-second outage exceeded the old three-second mobile buffer.
    // Drain that much PCM while no further network data can arrive.
    assert_frames(&new.render(44_100 * 5), position);
    position += 44_100 * 5;
    assert_eq!(harness.player.snapshot().underruns, 0);
    gate.release();
    new.wait_frames(capacity);
    assert_frames(&new.render(4_410), position);
    assert_eq!(harness.player.snapshot().underruns, 0);
    assert!(harness.player.event().is_none());
}

fn assert_frames(samples: &[f32], start: usize) {
    for (index, frame) in samples.chunks_exact(2).enumerate() {
        assert_eq!(frame[0], ((start + index) % 1_000) as f32 / 32_768.0);
        assert_eq!(frame[1], -frame[0]);
    }
}

#[test]
fn repeated_mobile_pauses_retain_only_unplayed_audio_after_buffer_rollover() {
    let (_dir, track) = tests::fixture(44_100, 44_100 * 20);
    let harness = Harness::with_policy(Some(Device::new("first", 44_100)), None, true);
    harness.player.play(vec![track], 0);
    let capacity = output::buffer_capacity(44_100, true);
    let mut position = 0;
    for attempt in 0..3 {
        let session = harness.session(attempt);
        session.wait_frames(capacity);
        assert_frames(&session.render(44_100), position);
        position += 44_100;
        session.wait_frames(capacity);
        harness.player.set_playing(false);
        wait(|| !session.alive.load(Ordering::Acquire));
        assert_eq!(
            harness.player.shared.position.load(Ordering::Relaxed),
            position as u64
        );
        harness.player.set_playing(true);
    }
    let resumed = harness.session(3);
    resumed.wait_frames(4_410);
    assert_frames(&resumed.render(4_410), position);
    assert!(harness.player.event().is_none());
}

#[test]
fn mobile_resume_discards_retained_pcm_when_sample_rate_changes() {
    let (_dir, track) = tests::fixture(44_100, 88_200);
    let harness = Harness::with_policy(Some(Device::new("first", 44_100)), None, true);
    harness.player.play(vec![track.clone()], 0);
    let old = harness.session(0);
    old.wait_frames(8_820);
    old.render(4_410);
    harness.player.set_playing(false);
    wait(|| !old.alive.load(Ordering::Acquire));
    harness.device(Some(Device::new("48khz", 48_000)));
    harness.player.set_playing(true);
    let new = harness.session(1);
    new.wait_frames(32);
    assert_eq!(harness.player.snapshot().seconds, 0.1);
    let mut reference = Converted::new(&track, 4_800, 48_000, None).unwrap();
    for frame in new.render(32).chunks_exact(2) {
        assert_eq!(frame, reference.next().unwrap().unwrap());
    }
    assert!(harness.player.event().is_none());
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
            release_when_paused: false,
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

#[test]
fn system_pause_is_visible_while_network_read_is_blocked_and_never_auto_resumes() {
    let (_dir, mut track) = tests::fixture(44_100, 44_100 * 40);
    let MediaAccess::File { locator } = &track.access else {
        unreachable!()
    };
    let gate = Arc::new(NetworkGate::default());
    let resolver = Arc::new(NetworkResolver {
        bytes: std::fs::read(locator).unwrap(),
        gate: gate.clone(),
        opens: AtomicUsize::new(0),
    });
    track.access = MediaAccess::Managed {
        provider: "network-test".into(),
        item: "track".into(),
    };
    let harness =
        Harness::with_resolver(Some(Device::new("car", 44_100)), None, true, Some(resolver));
    harness.player.play(vec![track], 0);
    let output = harness.session(0);
    output.wait_frames(output::buffer_capacity(44_100, true));
    *gate.blocked.lock().unwrap() = true;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !gate.entered.load(Ordering::Acquire) && Instant::now() < deadline {
        output.render(4_410);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(gate.entered.load(Ordering::Acquire));
    assert!(harness.player.snapshot().playing);
    harness.player.shared.pause_for_system_event();
    assert!(!harness.player.snapshot().playing);
    let position = harness.player.snapshot().seconds;
    assert!(output.render(4_410).iter().all(|sample| *sample == 0.0));
    assert_eq!(harness.player.snapshot().seconds, position);
    gate.release();
    wait(|| !output.alive.load(Ordering::Acquire));
    assert!(!harness.player.snapshot().playing);
    assert_eq!(harness.devices.lock().unwrap().sessions.len(), 1);
}

#[test]
fn progressive_append_resumes_natural_exhaustion_but_never_explicit_pause() {
    let (_dir, track) = tests::fixture(44_100, 100);
    let harness = Harness::with_policy(Some(Device::new("first", 44_100)), None, false);
    harness.player.play(vec![track.clone()], 0);
    let session = harness.session(0);
    session.wait_frames(100);
    session.render(100);
    wait(|| !harness.player.snapshot().playing);
    harness.player.append_stream(vec![track.clone()]);
    wait(|| harness.player.snapshot().playing);
    session.wait_frames(100);
    session.render(100);
    wait(|| !harness.player.snapshot().playing);
    harness.player.set_playing(false);
    // Queue command ordering is fenced by the next append: a deliberate Pause
    // while waiting for files must prevent a later discovery from resuming audio.
    let tracks = vec![track.clone(); 3];
    harness.player.append_stream(vec![track]);
    let update = harness
        .player
        .edit_queue(Box::new(move |_| (tracks, None)))
        .unwrap();
    assert!(!update.clock.playing);
}

struct CancelInputResolver {
    opening: bool,
    opens: AtomicUsize,
    tokens: Mutex<Vec<std::sync::Weak<AtomicBool>>>,
    entered: mpsc::Sender<()>,
}
struct CancelInputReader {
    token: Arc<AtomicBool>,
    entered: Option<mpsc::Sender<()>>,
    frame: u64,
}
impl PcmReader for CancelInputReader {
    fn format(&self) -> PcmFormat {
        PcmFormat::new(44100, 1).unwrap()
    }
    fn total_frames(&self) -> Option<u64> {
        Some(441000)
    }
    fn seek_exact(&mut self, frame: FramePosition) -> Result<()> {
        self.frame = frame.0;
        Ok(())
    }
    fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            while !self.token.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        }
        if self.token.load(Ordering::Acquire) {
            bail!("Native read canceled");
        }
        if self.frame >= 441000 {
            return Ok(None);
        }
        let count = max.min((441000 - self.frame) as usize);
        let chunk = PcmChunk::new(self.format(), FramePosition(self.frame), vec![0.25; count])?;
        self.frame += count as u64;
        Ok(Some(chunk))
    }
}
impl MediaResolver for CancelInputResolver {
    fn resolve(&self, _: &Track) -> Result<ByteSource> {
        bail!("Decoded input")
    }
    fn open(&self, _: &Track) -> Result<MediaInput> {
        let first = self.opens.fetch_add(1, Ordering::AcqRel) == 0;
        let token = Arc::new(AtomicBool::new(false));
        self.tokens.lock().unwrap().push(Arc::downgrade(&token));
        if first && self.opening {
            self.entered.send(()).unwrap();
            while !token.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            bail!("Native initialization canceled");
        }
        Ok(MediaInput::Decoded(Box::new(CancelInputReader {
            token,
            entered: (first && !self.opening).then(|| self.entered.clone()),
            frame: 0,
        })))
    }
    fn cancel(&self) {
        self.tokens.lock().unwrap().retain(|weak| {
            if let Some(token) = weak.upgrade() {
                token.store(true, Ordering::Release);
                true
            } else {
                false
            }
        });
    }
}
#[test]
fn seek_and_queue_edits_cancel_blocked_inputs_without_pausing_or_reporting_decode_errors() {
    for opening in [false, true] {
        for edit in [false, true] {
            let (_directory, mut track) = tests::fixture(44100, 1);
            track.duration_seconds = 10.0;
            track.access = MediaAccess::Managed {
                provider: "native".into(),
                item: "track".into(),
            };
            let devices = Arc::new(Mutex::new(Devices {
                current: Some(Device::new("output", 44100)),
                ..Default::default()
            }));
            let (entered, waiting) = mpsc::channel();
            let resolver = Arc::new(CancelInputResolver {
                opening,
                opens: AtomicUsize::new(0),
                tokens: Mutex::default(),
                entered,
            });
            let (player, worker) = Player::spawn(
                Some(resolver.clone()),
                FakeBackend {
                    devices: devices.clone(),
                    gate: None,
                    release_when_paused: false,
                },
            )
            .unwrap();
            player.play(vec![track.clone()], 0);
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            if edit {
                let update = player
                    .edit_queue(Box::new(move |_| (vec![track], None)))
                    .unwrap();
                assert!(update.clock.playing);
            } else {
                player.seek(0.5);
            }
            wait(|| resolver.opens.load(Ordering::Acquire) >= 2);
            let session = devices.lock().unwrap().sessions.last().unwrap().clone();
            session.wait_frames(128);
            assert!(
                player.snapshot().playing,
                "Transport cancellation must retain play intent"
            );
            if !edit {
                assert_eq!(player.snapshot().seconds, 0.5);
            }
            assert_eq!(session.render(128)[0], 0.25);
            assert!(
                player.event().is_none(),
                "Cancellation is not a decode failure"
            );
            drop(player);
            worker.join().unwrap();
        }
    }
}

#[test]
fn live_crossfeed_changes_already_buffered_audio_without_transport_edits() {
    let (_dir, track) = tests::fixture(44_100, 44_100);
    let harness = Harness::new(Some(Device::new("headphones", 44_100)));
    let session = harness.session(0);
    harness.player.play(vec![track], 0);
    session.wait_frames(6000);
    let generation = session.shared.generation.load(Ordering::Acquire);
    let before = harness.player.snapshot();
    let dry = session.render(500);
    for (i, frame) in dry.chunks_exact(2).enumerate() {
        assert_eq!(frame, [i as f32 / 32768.0, -(i as f32 / 32768.0)]);
    }
    let settings = amp_core::AudioProcessingSettings {
        crossfeed: amp_core::CrossfeedSettings {
            enabled: true,
            ..Default::default()
        },
    };
    harness.player.set_processing(settings);
    let wet = session.render(1000);
    assert_ne!(wet[1998], 499.0 / 32768.0);
    assert!(wet[1998].is_finite());
    harness.player.set_processing(Default::default());
    let disabled = session.render(1000);
    // The tail is an exact dry bypass once the 441-frame fade completes.
    for (i, frame) in disabled.chunks_exact(2).enumerate().skip(441) {
        let expected = ((1500 + i) % 1000) as f32 / 32768.0;
        assert_eq!(frame, [expected, -expected]);
    }
    assert_eq!(
        session.shared.generation.load(Ordering::Acquire),
        generation
    );
    let after = harness.player.snapshot();
    assert_eq!(after.sequence, before.sequence);
    assert_eq!(after.track, before.track);
    assert!(after.playing);
    assert_eq!(after.seconds, 2500.0 / 44100.0);
}

#[test]
fn enabled_crossfeed_rebuilds_at_new_output_rate_and_keeps_latest_paused_settings() {
    let (_dir, track) = tests::fixture(44_100, 88_200);
    let harness = Harness::with_policy(Some(Device::new("first", 44_100)), None, true);
    harness
        .player
        .set_processing(amp_core::AudioProcessingSettings {
            crossfeed: amp_core::CrossfeedSettings {
                enabled: true,
                ..Default::default()
            },
        });
    harness.player.play(vec![track.clone()], 0);
    let old = harness.session(0);
    old.wait_frames(8820);
    old.render(4410);
    harness.player.set_playing(false);
    wait(|| !old.alive.load(Ordering::Acquire));
    let settings = amp_core::AudioProcessingSettings {
        crossfeed: amp_core::CrossfeedSettings {
            enabled: true,
            cutoff_hz: 1300,
            feed_db_tenths: 103,
        },
    };
    harness.player.set_processing(settings);
    harness.device(Some(Device::new("48khz", 48_000)));
    harness.player.set_playing(true);
    let new = harness.session(1);
    new.wait_frames(1000);
    assert_eq!(harness.player.snapshot().seconds, 0.1);
    let mut reader = Converted::new(&track, 4800, 48_000, None).unwrap();
    let mut processing = processing::ProcessingChain::new(48_000, 2);
    processing.begin_callback(1, processing::pack(settings));
    for frame in new.render(1000).chunks_exact(2) {
        assert_eq!(
            frame,
            processing.process_frame(reader.next().unwrap().unwrap())
        );
    }
}
