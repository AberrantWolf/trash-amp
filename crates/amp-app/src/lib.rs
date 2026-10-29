//! Application playback service. Commands, queue policy and engine monitoring do
//! not depend on a window, event loop, provider UI or native media framework.
pub mod catalog;
pub mod files;

use amp_audio::{MediaResolver, OutputState, Player, PlayerEvent};
use amp_core::{Album, Library, Track};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek(f64),
    SeekFraction(f64),
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub revision: u64,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub current: Option<Track>,
    pub up_next: Arc<[Track]>,
    pub playing: bool,
    pub seconds: f64,
    pub level: f32,
    pub output: OutputState,
    pub notice: Option<Notice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlayingAlbum {
    pub title: String,
    /// Provider-cached local artwork; native hosts must tolerate missing files.
    pub artwork_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub album: Option<Arc<NowPlayingAlbum>>,
    pub track: Option<Track>,
    pub playing: bool,
    pub seconds: f64,
}

pub type NowPlayingObserver = Arc<dyn Fn(NowPlaying) + Send + Sync>;
pub type PreparePlayback = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// Replaceable engine boundary. Platform-managed playback can implement its own
/// engine without bringing a native framework into the application service.
pub trait Engine: Send {
    /// Return a replacement ID; snapshots echo it once that sequence is applied.
    fn play(&self, tracks: Vec<Track>, start: usize) -> u64;
    fn set_playing(&self, playing: bool);
    fn toggle(&self);
    fn seek(&self, seconds: f64);
    fn next(&self);
    fn previous(&self);
    fn append(&self, tracks: Vec<Track>);
    fn append_stream(&self, tracks: Vec<Track>) {
        self.append(tracks);
    }
    fn truncate(&self, len: usize);
    /// Apply policy atomically against the audible index, after prior commands.
    fn edit_queue(&self, edit: amp_audio::QueueEdit) -> Result<amp_audio::QueueUpdate>;
    fn snapshot(&self) -> amp_audio::Snapshot;
    fn event(&self) -> Option<PlayerEvent>;
}

impl Engine for Player {
    fn play(&self, tracks: Vec<Track>, start: usize) -> u64 {
        self.play(tracks, start)
    }
    fn set_playing(&self, playing: bool) {
        self.set_playing(playing);
    }
    fn toggle(&self) {
        self.toggle();
    }
    fn seek(&self, seconds: f64) {
        self.seek(seconds);
    }
    fn next(&self) {
        self.next();
    }
    fn previous(&self) {
        self.previous();
    }
    fn append(&self, tracks: Vec<Track>) {
        self.append(tracks);
    }
    fn append_stream(&self, tracks: Vec<Track>) {
        self.append_stream(tracks);
    }
    fn truncate(&self, len: usize) {
        self.truncate(len);
    }
    fn edit_queue(&self, edit: amp_audio::QueueEdit) -> Result<amp_audio::QueueUpdate> {
        self.edit_queue(edit)
    }
    fn snapshot(&self) -> amp_audio::Snapshot {
        self.snapshot()
    }
    fn event(&self) -> Option<PlayerEvent> {
        self.event()
    }
}

pub type EngineFactory =
    Box<dyn Fn(Option<Arc<dyn MediaResolver>>, u32) -> Result<Box<dyn Engine>> + Send>;

enum Command {
    Resolver(Arc<dyn MediaResolver>),
    BufferSeconds(u32),
    DefaultAlbum(Option<(Vec<Track>, bool)>),
    Album(Vec<Track>, usize),
    Track(Track),
    List(Vec<Track>),
    ReplaceQueue(Vec<Track>),
    FileDiscovery(String, Arc<AtomicBool>),
    FileQueue(Track, Arc<AtomicBool>),
    FileQueueAppend(String, Vec<Track>),
    FileQueueFinished(String),
    Enqueue(Track),
    ClearQueue,
    Metadata(Album),
    CatalogMetadata(Arc<Library>, Arc<BTreeMap<String, String>>),
    Transport(Transport),
    Observer(Option<NowPlayingObserver>),
    Shutdown,
    #[cfg(test)]
    Barrier(mpsc::Sender<()>),
}

/// Cheap, thread-safe command handle. UI and native media callbacks use the
/// same commands. Reading a snapshot never waits for decoding or native calls.
#[derive(Clone)]
pub struct Playback {
    commands: mpsc::Sender<Command>,
    snapshot: Arc<RwLock<Snapshot>>,
}
impl Playback {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().unwrap().clone()
    }
    /// Configure the next engine; existing playback keeps its current allocation.
    pub fn set_buffer_seconds(&self, seconds: u32) {
        self.send(Command::BufferSeconds(seconds.clamp(3, 60)));
    }
    /// Install the stable resolving adapter before the first playback request.
    pub fn set_resolver(&self, resolver: Arc<dyn MediaResolver>) {
        self.send(Command::Resolver(resolver));
    }
    pub fn set_default_album(&self, tracks: Option<(Vec<Track>, bool)>) {
        self.send(Command::DefaultAlbum(tracks));
    }
    pub fn play_album(&self, tracks: Vec<Track>, start: usize) {
        self.send(Command::Album(tracks, start));
    }
    pub fn play_track(&self, track: Track) {
        self.send(Command::Track(track));
    }
    pub fn play_list(&self, tracks: Vec<Track>) {
        self.send(Command::List(tracks));
    }
    /// Start a transient queue, exposing every remaining track in Up next.
    /// Replaces the current sequence and any pending explicit tracks.
    pub fn replace_queue(&self, tracks: Vec<Track>) {
        self.send(Command::ReplaceQueue(tracks));
    }
    /// Start discovery-owned playback; cancellation prevents stale queued commands.
    pub fn begin_file_discovery(&self, session: String, cancelled: Arc<AtomicBool>) {
        self.send(Command::FileDiscovery(session, cancelled));
    }
    pub fn start_file_queue(&self, track: Track, cancelled: Arc<AtomicBool>) {
        self.send(Command::FileQueue(track, cancelled));
    }
    pub fn finish_file_queue(&self, session: String) {
        self.send(Command::FileQueueFinished(session));
    }
    pub fn append_file_queue(&self, session: String, tracks: Vec<Track>) {
        self.send(Command::FileQueueAppend(session, tracks));
    }
    pub fn enqueue(&self, track: Track) {
        self.send(Command::Enqueue(track));
    }
    pub fn clear_queue(&self) {
        self.send(Command::ClearQueue);
    }
    pub fn update_metadata(&self, album: Album) {
        self.send(Command::Metadata(album));
    }
    /// Refresh names and native album context without changing media or queue order.
    pub fn refresh_catalog_metadata(
        &self,
        library: Arc<Library>,
        artwork: Arc<BTreeMap<String, String>>,
    ) {
        self.send(Command::CatalogMetadata(library, artwork));
    }
    pub fn transport(&self, command: Transport) {
        self.send(Command::Transport(command));
    }
    pub fn observe_now_playing(&self, observer: Option<NowPlayingObserver>) {
        self.send(Command::Observer(observer));
    }
    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// Own this independently of any presentation. Dropping the owner shuts down
/// the orchestration worker even when native callbacks retain command handles.
pub struct PlaybackService {
    playback: Playback,
    worker: Option<thread::JoinHandle<()>>,
}
impl PlaybackService {
    pub fn new(prepare: PreparePlayback) -> Result<Self> {
        Self::with_engine_factory(
            prepare,
            Box::new(|resolver, seconds| {
                Ok(Box::new(Player::with_resolver_and_buffer(
                    resolver, seconds,
                )?))
            }),
        )
    }
    /// Inject a playback backend without changing queue or transport orchestration.
    pub fn with_engine_factory(prepare: PreparePlayback, factory: EngineFactory) -> Result<Self> {
        let (commands, rx) = mpsc::channel();
        let snapshot = Arc::new(RwLock::new(Snapshot::default()));
        let shared = snapshot.clone();
        let worker = thread::Builder::new()
            .name("amp-app".into())
            .spawn(move || {
                Worker::new(prepare, factory, shared).run(rx);
            })?;
        Ok(Self {
            playback: Playback { commands, snapshot },
            worker: Some(worker),
        })
    }
    pub fn playback(&self) -> Playback {
        self.playback.clone()
    }
}
impl Drop for PlaybackService {
    fn drop(&mut self) {
        self.playback.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct Queue {
    sequence: Vec<Track>,
    pending: Vec<Track>,
    explicit_start: usize,
}
impl Queue {
    fn remaining(&self, index: usize) -> Vec<Track> {
        self.sequence
            .iter()
            .skip(self.explicit_start.max(index.saturating_add(1)))
            .chain(&self.pending)
            .cloned()
            .collect()
    }
    fn metadata(&mut self, album: &Album) {
        for track in self.sequence.iter_mut().chain(&mut self.pending) {
            if let Some(updated) = album.tracks.iter().find(|t| t.id == track.id) {
                track.title.clone_from(&updated.title);
                track.artist.clone_from(&updated.artist);
            }
        }
    }
}

struct Worker {
    prepare: PreparePlayback,
    factory: EngineFactory,
    resolver: Option<Arc<dyn MediaResolver>>,
    engine: Option<Box<dyn Engine>>,
    buffer_seconds: u32,
    default_album: Option<(Vec<Track>, bool)>,
    queue: Queue,
    file_queue: Option<(String, Arc<AtomicBool>)>,
    clock: amp_audio::Snapshot,
    expected_sequence: u64,
    published: Arc<RwLock<Snapshot>>,
    notice: Option<Notice>,
    observer: Option<NowPlayingObserver>,
    last_media: Option<MediaIdentity>,
    albums: BTreeMap<TrackKey, Arc<NowPlayingAlbum>>,
    up_next: Arc<[Track]>,
    queue_dirty: bool,
}
type TrackKey = (String, String, String);
fn track_key(track: &Track) -> TrackKey {
    (
        track.id.library.clone(),
        track.id.provider.clone(),
        track.id.item.clone(),
    )
}
type MediaIdentity = Option<(
    amp_core::ItemId,
    String,
    String,
    u64,
    bool,
    u64,
    Option<Arc<NowPlayingAlbum>>,
)>;
impl Worker {
    fn new(
        prepare: PreparePlayback,
        factory: EngineFactory,
        published: Arc<RwLock<Snapshot>>,
    ) -> Self {
        Self {
            prepare,
            factory,
            resolver: None,
            engine: None,
            buffer_seconds: amp_core::default_audio_buffer_seconds(),
            default_album: None,
            queue: Queue::default(),
            file_queue: None,
            clock: amp_audio::Snapshot::default(),
            expected_sequence: 0,
            published,
            notice: None,
            observer: None,
            last_media: None,
            albums: BTreeMap::new(),
            up_next: Arc::from([]),
            queue_dirty: true,
        }
    }
    fn run(mut self, rx: mpsc::Receiver<Command>) {
        loop {
            let command = if self.engine.is_some() {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                }
            };
            if let Some(command) = command {
                if matches!(command, Command::Shutdown) {
                    break;
                }
                self.command(command);
            }
            // A bounded batch keeps engine events and publication moving under load.
            let mut shutdown = false;
            for _ in 0..63 {
                let Ok(command) = rx.try_recv() else { break };
                if matches!(command, Command::Shutdown) {
                    shutdown = true;
                    break;
                }
                self.command(command);
            }
            if shutdown {
                break;
            }
            self.poll();
        }
        if let Some(engine) = self.engine.take() {
            engine.set_playing(false);
        }
        self.clock.playing = false;
        self.clock.level = 0.0;
        self.poll();
    }
    fn command(&mut self, command: Command) {
        if matches!(
            &command,
            Command::Album(..)
                | Command::Track(..)
                | Command::List(..)
                | Command::ReplaceQueue(..)
                | Command::ClearQueue
        ) {
            self.cancel_file_queue();
        }
        let result = match command {
            Command::Resolver(resolver) => {
                if self.engine.is_some() {
                    Err(anyhow::anyhow!(
                        "Cannot replace the media resolver while an engine is active"
                    ))
                } else {
                    self.resolver = Some(resolver);
                    Ok(())
                }
            }
            Command::BufferSeconds(seconds) => {
                self.buffer_seconds = seconds;
                Ok(())
            }
            Command::DefaultAlbum(album) => {
                self.default_album = album;
                Ok(())
            }
            Command::Album(tracks, start) => self.play_album(tracks, start),
            Command::Track(track) => self.play_track(track),
            Command::List(tracks) => {
                let len = tracks.len();
                self.replace(tracks, 0, len)
            }
            Command::ReplaceQueue(tracks) => self.replace(tracks, 0, 1),
            Command::FileDiscovery(session, cancelled) => {
                self.cancel_file_queue();
                if !cancelled.load(Ordering::Relaxed) {
                    self.file_queue = Some((session, cancelled));
                }
                Ok(())
            }
            Command::FileQueue(track, cancelled) => self.start_file_queue(track, cancelled),
            Command::FileQueueFinished(session) => {
                if self
                    .file_queue
                    .as_ref()
                    .is_some_and(|(id, _)| *id == session)
                {
                    self.file_queue.take();
                }
                Ok(())
            }
            Command::FileQueueAppend(session, tracks) => {
                if self
                    .file_queue
                    .as_ref()
                    .is_some_and(|(id, stop)| *id == session && !stop.load(Ordering::Relaxed))
                {
                    if let Some(engine) = &self.engine {
                        self.queue.sequence.extend(tracks.iter().cloned());
                        engine.append_stream(tracks);
                        self.queue_dirty = true;
                    }
                }
                Ok(())
            }
            Command::Enqueue(track) => {
                if let Some(engine) = &self.engine {
                    self.queue.sequence.push(track.clone());
                    engine.append(vec![track]);
                } else {
                    self.queue.pending.push(track);
                }
                self.queue_dirty = true;
                Ok(())
            }
            Command::ClearQueue => {
                self.queue.pending.clear();
                if let Some(engine) = &self.engine {
                    let mut tracks = self.queue.sequence.clone();
                    let explicit = self.queue.explicit_start;
                    match engine.edit_queue(Box::new(move |index| {
                        tracks.truncate(explicit.max(index.saturating_add(1)));
                        (tracks, None)
                    })) {
                        Ok(update) => self.accept_edit(update),
                        Err(error) => self.notice(error.to_string()),
                    }
                }
                self.queue_dirty = true;
                Ok(())
            }
            Command::Metadata(album) => {
                for track in &album.tracks {
                    if let Some(info) = self.albums.get_mut(&track_key(track)) {
                        Arc::make_mut(info).title.clone_from(&album.title);
                    }
                }
                self.queue.metadata(&album);
                if let Some((tracks, _)) = &mut self.default_album {
                    for track in tracks {
                        if let Some(updated) = album.tracks.iter().find(|t| t.id == track.id) {
                            track.title.clone_from(&updated.title);
                            track.artist.clone_from(&updated.artist);
                        }
                    }
                }
                self.queue_dirty = true;
                Ok(())
            }
            Command::CatalogMetadata(library, artwork) => {
                // Keep album context for queued tracks when the user browses another
                // source, without retaining every previously visited catalog.
                let retained = self
                    .queue
                    .sequence
                    .iter()
                    .chain(&self.queue.pending)
                    .chain(self.default_album.iter().flat_map(|(tracks, _)| tracks))
                    .map(track_key)
                    .collect::<BTreeSet<_>>();
                self.albums.retain(|key, _| retained.contains(key));
                for album in &library.albums {
                    let info = Arc::new(NowPlayingAlbum {
                        title: album.title.clone(),
                        artwork_path: artwork.get(&album.id.item).cloned(),
                    });
                    for track in &album.tracks {
                        self.albums.insert(track_key(track), info.clone());
                    }
                }
                let names = library
                    .albums
                    .iter()
                    .flat_map(|album| &album.tracks)
                    .map(|track| {
                        (
                            (
                                track.id.library.as_str(),
                                track.id.provider.as_str(),
                                track.id.item.as_str(),
                            ),
                            track,
                        )
                    })
                    .collect::<std::collections::BTreeMap<_, _>>();
                let defaults = self
                    .default_album
                    .iter_mut()
                    .flat_map(|(tracks, _)| tracks.iter_mut());
                for track in self
                    .queue
                    .sequence
                    .iter_mut()
                    .chain(&mut self.queue.pending)
                    .chain(defaults)
                {
                    if let Some(updated) = names.get(&(
                        track.id.library.as_str(),
                        track.id.provider.as_str(),
                        track.id.item.as_str(),
                    )) {
                        track.title.clone_from(&updated.title);
                        track.artist.clone_from(&updated.artist);
                    }
                }
                self.queue_dirty = true;
                Ok(())
            }
            Command::Transport(transport) => self.transport(transport),
            Command::Observer(observer) => {
                self.observer = observer;
                self.last_media = None;
                Ok(())
            }
            Command::Shutdown => unreachable!(),
            #[cfg(test)]
            Command::Barrier(reply) => {
                self.poll();
                let _ = reply.send(());
                Ok(())
            }
        };
        if let Err(error) = result {
            self.notice(error.to_string());
        }
    }
    fn ensure_engine(&mut self) -> Result<()> {
        (self.prepare)()?;
        if self.engine.is_none() {
            self.engine = Some((self.factory)(self.resolver.clone(), self.buffer_seconds)?);
        }
        Ok(())
    }
    fn accept_edit(&mut self, update: amp_audio::QueueUpdate) {
        self.expected_sequence = update.clock.sequence;
        self.clock = update.clock;
        self.queue.sequence = update.tracks;
        self.queue_dirty = true;
    }
    fn cancel_file_queue(&mut self) {
        if let Some((_, stop)) = self.file_queue.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }
    fn start_file_queue(&mut self, track: Track, cancelled: Arc<AtomicBool>) -> Result<()> {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let session = track.id.library.clone();
        if !self
            .file_queue
            .as_ref()
            .is_some_and(|(id, stop)| *id == session && Arc::ptr_eq(stop, &cancelled))
        {
            self.cancel_file_queue();
        }
        if let Err(error) = self.replace(vec![track], 0, 1) {
            cancelled.store(true, Ordering::Relaxed);
            return Err(error);
        }
        self.file_queue = Some((session, cancelled));
        Ok(())
    }
    fn play_track(&mut self, track: Track) -> Result<()> {
        if self.engine.is_none() {
            let mut tracks = vec![track];
            tracks.extend(self.queue.pending.iter().cloned());
            return self.replace(tracks, 0, 1);
        }
        (self.prepare)()?;
        let queue = Queue {
            sequence: self.queue.sequence.clone(),
            pending: self.queue.pending.clone(),
            explicit_start: self.queue.explicit_start,
        };
        let update = self
            .engine
            .as_ref()
            .unwrap()
            .edit_queue(Box::new(move |index| {
                let mut tracks = vec![track];
                tracks.extend(queue.remaining(index));
                (tracks, Some(0))
            }))?;
        self.accept_edit(update);
        self.queue.pending.clear();
        self.queue.explicit_start = 1;
        Ok(())
    }
    fn replace(&mut self, tracks: Vec<Track>, start: usize, explicit_start: usize) -> Result<()> {
        if tracks.is_empty() || start >= tracks.len() {
            bail!("Choose a playable track");
        }
        self.ensure_engine()?;
        self.expected_sequence = self.engine.as_ref().unwrap().play(tracks.clone(), start);
        self.queue = Queue {
            sequence: tracks,
            pending: vec![],
            explicit_start,
        };
        self.clock.track = start;
        self.clock.seconds = 0.0;
        self.clock.playing = true;
        self.queue_dirty = true;
        Ok(())
    }
    fn play_album(&mut self, mut tracks: Vec<Track>, start: usize) -> Result<()> {
        if start >= tracks.len() {
            bail!("Choose a playable track");
        }
        let len = tracks.len();
        tracks.extend(self.queue.pending.iter().cloned());
        self.replace(tracks, start, len)
    }
    fn transport(&mut self, transport: Transport) -> Result<()> {
        if matches!(transport, Transport::Play | Transport::Toggle)
            && self.queue.sequence.is_empty()
        {
            let Some((tracks, ambiguous)) = self.default_album.clone() else {
                return Ok(());
            };
            if ambiguous {
                bail!("Choose a source disc before playing this album");
            }
            return self.play_album(tracks, 0);
        }
        if self.engine.is_none() {
            return Ok(());
        }
        if matches!(transport, Transport::Play)
            || (matches!(transport, Transport::Toggle) && !self.clock.playing)
        {
            (self.prepare)()?;
        }
        let engine = self.engine.as_ref().unwrap();
        match transport {
            Transport::Play => engine.set_playing(true),
            Transport::Pause => engine.set_playing(false),
            Transport::Toggle => engine.toggle(),
            Transport::Stop => {
                engine.set_playing(false);
                engine.seek(0.0);
            }
            Transport::Next => engine.next(),
            Transport::Previous => engine.previous(),
            Transport::Seek(seconds) if seconds.is_finite() => engine.seek(seconds.max(0.0)),
            Transport::SeekFraction(fraction) if fraction.is_finite() => {
                if let Some(track) = self.queue.sequence.get(self.clock.track) {
                    engine.seek(fraction.clamp(0.0, 1.0) * track.duration_seconds.max(0.0));
                }
            }
            Transport::Seek(_) | Transport::SeekFraction(_) => {}
        }
        Ok(())
    }
    fn notice(&mut self, message: String) {
        let revision = self.notice.as_ref().map_or(1, |n| n.revision + 1);
        self.notice = Some(Notice { revision, message });
    }
    fn poll(&mut self) {
        if let Some(engine) = &self.engine {
            let clock = engine.snapshot();
            // Never interpret the previous sequence's track index against a new list.
            if clock.sequence == self.expected_sequence {
                self.queue_dirty |= clock.track != self.clock.track;
                self.clock = clock;
            }
            let mut events = vec![];
            while let Some(event) = engine.event() {
                events.push(event);
            }
            for event in events {
                let message = match event {
                    PlayerEvent::OutputUnavailable(error) | PlayerEvent::PlaybackError(error) => {
                        error
                    }
                    PlayerEvent::OutputRecovered if self.clock.playing => {
                        "Audio output reconnected. Playing.".into()
                    }
                    PlayerEvent::OutputRecovered => {
                        "Audio output reconnected. Press Play to resume.".into()
                    }
                };
                self.notice(message);
            }
        }
        if self.queue_dirty {
            self.up_next = self.queue.remaining(self.clock.track).into();
            self.queue_dirty = false;
        }
        let current = self.queue.sequence.get(self.clock.track).cloned();
        let snapshot = Snapshot {
            current: current.clone(),
            up_next: self.up_next.clone(),
            playing: self.clock.playing,
            seconds: self.clock.seconds,
            level: self.clock.level,
            output: self.clock.output,
            notice: self.notice.clone(),
        };
        *self.published.write().unwrap() = snapshot;
        let album = current
            .as_ref()
            .and_then(|track| self.albums.get(&track_key(track)))
            .cloned();
        let identity = current.as_ref().map(|track| {
            (
                track.id.clone(),
                track.title.clone(),
                track.artist.clone(),
                track.duration_seconds.to_bits(),
                self.clock.playing,
                self.clock.seconds.max(0.0) as u64,
                album.clone(),
            )
        });
        if let Some(observer) = &self.observer
            && self.last_media.as_ref() != Some(&identity)
        {
            self.last_media = Some(identity);
            observer(NowPlaying {
                album,
                track: current,
                playing: self.clock.playing,
                seconds: self.clock.seconds,
            });
        }
    }
}

#[cfg(test)]
mod tests;
