//! Streaming PCM pipeline. The device callback uses bounded ring reads, atomics and allocation-free DSP.
use amp_core::{MediaAccess, Track};
use anyhow::{Context, Result, bail};
use junk_libs_audio::{AudioDecoder, CuePcmDecoder, RedumperPcmDecoder, SincResampler, WavDecoder};
pub use junk_libs_audio::{ByteSource, FramePosition, MediaHint, PcmChunk, PcmFormat};
use junk_libs_disc::{
    cue_audio::CueAudioDisc,
    redumper::{
        find_sidecars,
        pcm::{InvalidSamplePolicy, RawAudioReader},
    },
};
use junk_libs_playback::{Generation, QueueState, QueueTrack};
use std::{
    collections::VecDeque,
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

mod input;
#[cfg(target_os = "ios")]
mod ios_session;
mod output;
mod processing;
pub use input::{MediaInput, PcmReader, ResolverRegistry};

#[cfg(test)]
mod recovery_tests;
mod worker;

#[derive(Clone, Copy, Default)]
struct Frame {
    samples: [f32; 2],
    generation: u64,
    track: usize,
    position: u64,
}
#[derive(Default)]
struct Shared {
    clock_writer: AtomicBool,
    playing: AtomicBool,
    // High bits order transport requests; low bit gates rendering immediately.
    transport: AtomicU64,
    pause_leases: AtomicUsize,
    generation: AtomicU64,
    track: AtomicUsize,
    position: AtomicU64,
    level: AtomicU32,
    underruns: AtomicU64,
    shutdown: AtomicBool,
    input_revision: AtomicU64,
    input_changes: AtomicUsize,
    sample_rate: AtomicU32,
    buffer_seconds: u32,
    sequence: AtomicU64,
    output_state: AtomicUsize,
    processing: AtomicU32,
}
impl Shared {
    /// A native notification must stop rendering and publish pause even when
    /// the decoder worker is waiting for a network read.
    #[cfg(any(target_os = "ios", test))]
    fn pause_for_system_event(&self) {
        self.playing.store(false, Ordering::Relaxed);
        self.level.store(0, Ordering::Relaxed);
    }
}
/// The render callback never waits. A decoder-side edit can briefly silence a
/// callback while it captures the audible position and installs a new generation.
struct ClockGuard<'a>(&'a AtomicBool);
impl<'a> ClockGuard<'a> {
    fn try_lock(writer: &'a AtomicBool) -> Option<Self> {
        writer
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(writer))
    }
    fn lock(writer: &'a AtomicBool) -> Self {
        loop {
            if let Some(guard) = Self::try_lock(writer) {
                return guard;
            }
            thread::yield_now();
        }
    }
}
impl Drop for ClockGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
/// Availability of the current system-default output. Recovery never starts audio on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputState {
    #[default]
    Connecting,
    Ready,
    Unavailable,
}

pub enum PlayerEvent {
    OutputUnavailable(String),
    OutputRecovered,
    PlaybackError(String),
    TrackPlayable {
        sequence: u64,
        index: usize,
    },
    TrackUnplayable {
        sequence: u64,
        index: usize,
        reason: String,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The latest sequence replacement applied by the decoder worker.
    pub sequence: u64,
    pub output: OutputState,
    pub playing: bool,
    pub track: usize,
    pub seconds: f64,
    pub level: f32,
    pub underruns: u64,
}
/// Queue policy runs on the decoder worker against its audible position, after
/// earlier transport commands. None preserves that position; Some starts anew.
pub type QueueEdit = Box<dyn FnOnce(usize) -> (Vec<Track>, Option<usize>) + Send>;
pub struct QueueUpdate {
    pub tracks: Vec<Track>,
    pub clock: Snapshot,
}
enum Command {
    Shutdown,
    Edit(QueueEdit, u64, mpsc::Sender<Vec<Track>>),
    Replace(Vec<Track>, usize, u64),
    Seek(f64),
    Toggle(u64),
    SetPlaying(bool, u64),
    Next,
    Previous,
    Append(Vec<Track>, bool),
    Truncate(usize),
}
/// Adapter invoked only on the decoder worker, never in the device callback.
pub trait MediaResolver: Send + Sync {
    fn resolve(&self, track: &Track) -> Result<ByteSource>;
    fn open(&self, track: &Track) -> Result<MediaInput> {
        self.resolve(track).map(MediaInput::Encoded)
    }
    fn cancel(&self);
}
/// Acquiring a guard stops rendering until it is dropped. Application owners
/// retain it while a Pause command waits behind blocking queue edits.
pub type RenderPause = Arc<dyn Fn() -> Box<dyn Send> + Send + Sync>;
struct PauseLease(Arc<Shared>);
impl Drop for PauseLease {
    fn drop(&mut self) {
        self.0.pause_leases.fetch_sub(1, Ordering::Release);
    }
}

pub struct Player {
    resolver: Option<Arc<dyn MediaResolver>>,
    commands: mpsc::Sender<Command>,
    next_sequence: AtomicU64,
    shared: Arc<Shared>,
    events: mpsc::Receiver<PlayerEvent>,
}
/// Keep new native/remote reads out of the cancel-to-command publication gap.
struct InputChange<'a>(&'a Shared);
impl Drop for InputChange<'_> {
    fn drop(&mut self) {
        self.0.input_changes.fetch_sub(1, Ordering::Release);
    }
}
impl Player {
    /// Starts the audio worker. Output availability and recovery are reported asynchronously.
    pub fn new() -> Result<Self> {
        Self::with_resolver(None)
    }
    pub fn with_resolver(resolver: Option<Arc<dyn MediaResolver>>) -> Result<Self> {
        Self::spawn(resolver, output::CpalBackend).map(|(player, _worker)| player)
    }
    fn spawn<B: output::Backend>(
        resolver: Option<Arc<dyn MediaResolver>>,
        backend: B,
    ) -> Result<(Self, thread::JoinHandle<()>)> {
        Self::spawn_with_buffer(resolver, backend, 0)
    }
    /// Use a bounded read-ahead buffer, configured before the output worker starts.
    pub fn with_resolver_and_buffer(
        resolver: Option<Arc<dyn MediaResolver>>,
        seconds: u32,
    ) -> Result<Self> {
        Self::spawn_with_buffer(resolver, output::CpalBackend, seconds.clamp(3, 60))
            .map(|(player, _worker)| player)
    }
    fn spawn_with_buffer<B: output::Backend>(
        resolver: Option<Arc<dyn MediaResolver>>,
        backend: B,
        buffer_seconds: u32,
    ) -> Result<(Self, thread::JoinHandle<()>)> {
        let shared = Arc::new(Shared {
            buffer_seconds,
            processing: AtomicU32::new(processing::pack(Default::default())),
            sample_rate: AtomicU32::new(44_100),
            ..Shared::default()
        });
        let (commands, rx) = mpsc::channel();
        let (tx, events) = mpsc::channel();
        let state = shared.clone();
        let worker_resolver = resolver.clone();
        let worker = thread::Builder::new()
            .name("amp-decode".into())
            .spawn(move || worker::run(rx, tx, state, worker_resolver, backend))?;
        Ok((
            Self {
                resolver,
                commands,
                next_sequence: AtomicU64::new(0),
                shared,
                events,
            },
            worker,
        ))
    }
    fn changing_input(&self) -> InputChange<'_> {
        self.shared.input_changes.fetch_add(1, Ordering::AcqRel);
        self.shared.input_revision.fetch_add(1, Ordering::AcqRel);
        if let Some(resolver) = &self.resolver {
            resolver.cancel();
        }
        InputChange(&self.shared)
    }
    pub fn play(&self, tracks: Vec<Track>, start: usize) -> u64 {
        let _input_change = self.changing_input();
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self
            .commands
            .send(Command::Replace(tracks, start, sequence));
        sequence
    }
    /// Updates output effects without invalidating buffered audio or the transport.
    pub fn set_processing(&self, settings: amp_core::AudioProcessingSettings) {
        self.shared
            .processing
            .store(processing::pack(settings), Ordering::Release);
    }
    pub fn render_pause(&self) -> RenderPause {
        let shared = self.shared.clone();
        Arc::new(move || {
            shared.pause_leases.fetch_add(1, Ordering::AcqRel);
            shared.level.store(0, Ordering::Relaxed);
            Box::new(PauseLease(shared.clone()))
        })
    }
    fn request_transport(&self, pause: bool) -> u64 {
        let previous = self
            .shared
            .transport
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(((state >> 1).wrapping_add(1) << 1) | (state & 1) | u64::from(pause))
            })
            .unwrap();
        if pause {
            self.shared.level.store(0, Ordering::Relaxed);
        }
        (previous >> 1).wrapping_add(1)
    }
    pub fn toggle(&self) {
        let revision = self.request_transport(self.snapshot().playing);
        let _ = self.commands.send(Command::Toggle(revision));
    }
    pub fn set_playing(&self, playing: bool) {
        let revision = self.request_transport(!playing);
        let _ = self.commands.send(Command::SetPlaying(playing, revision));
    }
    pub fn seek(&self, seconds: f64) {
        let _input_change = self.changing_input();
        let _ = self.commands.send(Command::Seek(seconds));
    }
    pub fn next(&self) {
        let _input_change = self.changing_input();
        let _ = self.commands.send(Command::Next);
    }
    pub fn previous(&self) {
        let _input_change = self.changing_input();
        // Standard transport behavior: restart after the first few seconds.
        if self.snapshot().seconds > 3.0 {
            self.seek(0.0);
        } else {
            let _ = self.commands.send(Command::Previous);
        }
    }
    /// Continue a progressively discovered queue after natural exhaustion, preserving pause.
    pub fn append_stream(&self, tracks: Vec<Track>) {
        let _ = self.commands.send(Command::Append(tracks, true));
    }
    pub fn append(&self, tracks: Vec<Track>) {
        let _ = self.commands.send(Command::Append(tracks, false));
    }
    pub fn truncate(&self, len: usize) {
        let _ = self.commands.send(Command::Truncate(len));
    }
    pub fn edit_queue(&self, edit: QueueEdit) -> Result<QueueUpdate> {
        let _input_change = self.changing_input();
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        self.commands
            .send(Command::Edit(edit, sequence, tx))
            .map_err(|_| anyhow::anyhow!("Audio worker stopped during queue edit"))?;
        let tracks = rx
            .recv()
            .context("Audio worker stopped during queue edit")?;
        Ok(QueueUpdate {
            tracks,
            clock: self.snapshot(),
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            sequence: self.shared.sequence.load(Ordering::Acquire),
            output: match self.shared.output_state.load(Ordering::Acquire) {
                1 => OutputState::Ready,
                2 => OutputState::Unavailable,
                _ => OutputState::Connecting,
            },
            playing: self.shared.playing.load(Ordering::Relaxed)
                && self.shared.transport.load(Ordering::Acquire) & 1 == 0
                && self.shared.pause_leases.load(Ordering::Acquire) == 0,
            track: self.shared.track.load(Ordering::Relaxed),
            seconds: self.shared.position.load(Ordering::Relaxed) as f64
                / f64::from(self.shared.sample_rate.load(Ordering::Acquire).max(1)),
            level: f32::from_bits(self.shared.level.load(Ordering::Relaxed)),
            underruns: self.shared.underruns.load(Ordering::Relaxed),
        }
    }
    pub fn error(&self) -> Option<String> {
        while let Some(event) = self.event() {
            match event {
                PlayerEvent::OutputUnavailable(error) | PlayerEvent::PlaybackError(error) => {
                    return Some(error);
                }
                PlayerEvent::TrackUnplayable { reason, .. } => return Some(reason),
                PlayerEvent::OutputRecovered | PlayerEvent::TrackPlayable { .. } => {}
            }
        }
        None
    }
    /// Poll either events or errors; both consume the same notification queue.
    pub fn event(&self) -> Option<PlayerEvent> {
        self.events.try_recv().ok()
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(resolver) = &self.resolver {
            resolver.cancel();
        }
        let _ = self.commands.send(Command::Shutdown);
    }
}

struct AudioReader {
    source: Box<dyn PcmReader>,
    buffered: VecDeque<[f32; 2]>,
    rate: u32,
}
impl AudioReader {
    fn open(
        track: &Track,
        output_frame: u64,
        output_rate: u32,
        resolver: Option<&dyn MediaResolver>,
    ) -> Result<Self> {
        match &track.access {
            MediaAccess::Managed { provider, .. } => {
                let input = resolver
                    .context(format!("{provider}: playback adapter unavailable"))?
                    .open(track)?;
                let mut decoder: Box<dyn PcmReader> = match input {
                    MediaInput::Encoded(source) => Box::new(AudioDecoder::open(source)?),
                    MediaInput::Decoded(reader) => reader,
                };
                if decoder.format().channels() > 2 {
                    bail!("Multichannel playback requires an explicit channel mapper")
                }
                let rate = decoder.format().sample_rate();
                let frame = scale_frame(output_frame, output_rate, rate);
                if frame > 0 {
                    decoder.seek_exact(FramePosition(frame))?;
                }
                Ok(Self {
                    source: decoder,
                    buffered: VecDeque::new(),
                    rate,
                })
            }
            MediaAccess::DiscTrack { cue, number } => {
                let mut reader = CuePcmDecoder::new(
                    CueAudioDisc::open(std::path::Path::new(cue))?.track_reader(*number)?,
                );
                let source_frame = scale_frame(output_frame, output_rate, 44_100);
                reader.seek_exact(FramePosition(source_frame.min(reader.total_frames())))?;
                Ok(Self {
                    source: Box::new(reader),
                    buffered: VecDeque::new(),
                    rate: 44100,
                })
            }
            MediaAccess::RedumperTrack { anchor, number } => {
                let mut reader = RedumperPcmDecoder::new(RawAudioReader::open(
                    &find_sidecars(std::path::Path::new(anchor)),
                    *number,
                    InvalidSamplePolicy::Reject,
                )?);
                let source_frame = scale_frame(output_frame, output_rate, 44_100);
                reader.seek_exact(FramePosition(source_frame.min(reader.total_frames())))?;
                Ok(Self {
                    source: Box::new(reader),
                    buffered: VecDeque::new(),
                    rate: 44100,
                })
            }
            MediaAccess::File { locator } => {
                let path = std::path::Path::new(locator);
                let extension = path.extension().and_then(|value| value.to_str());
                if extension.is_some_and(|value| {
                    value.eq_ignore_ascii_case("wav") || value.eq_ignore_ascii_case("wave")
                }) {
                    let file = File::open(path)?;
                    let length = file.metadata().ok().map(|metadata| metadata.len());
                    let mut decoder = WavDecoder::open(ByteSource::new(
                        file,
                        MediaHint::new(length, extension, Some("audio/wav")),
                    ))?;
                    if decoder.format().channels() > 2 {
                        bail!("Multichannel playback requires an explicit channel mapper")
                    }
                    let source_frame =
                        scale_frame(output_frame, output_rate, decoder.format().sample_rate());
                    decoder.seek_exact(FramePosition(
                        decoder
                            .total_frames()
                            .map_or(source_frame, |total| source_frame.min(total)),
                    ))?;
                    return Ok(Self {
                        rate: decoder.format().sample_rate(),
                        source: Box::new(decoder),
                        buffered: VecDeque::new(),
                    });
                }

                let file = File::open(locator)?;
                let length = file.metadata()?.len();
                let mut decoder = AudioDecoder::open(ByteSource::new(
                    file,
                    MediaHint::new(Some(length), extension, None),
                ))?;
                if decoder.format().channels() > 2 {
                    bail!("Multichannel playback requires an explicit channel mapper")
                }
                let rate = decoder.format().sample_rate();
                let frame = scale_frame(output_frame, output_rate, rate);
                if frame > 0 {
                    decoder.seek_exact(FramePosition(frame))?;
                }
                let result = Self {
                    source: Box::new(decoder),
                    buffered: VecDeque::new(),
                    rate,
                };
                Ok(result)
            }
        }
    }
    fn next(&mut self) -> Result<Option<[f32; 2]>> {
        loop {
            if let Some(frame) = self.buffered.pop_front() {
                return Ok(Some(frame));
            }
            let Some(chunk) = self.source.read_frames(1_024)? else {
                return Ok(None);
            };
            push_stereo(&mut self.buffered, chunk)?;
        }
    }
}

fn push_stereo(buffered: &mut VecDeque<[f32; 2]>, chunk: PcmChunk) -> Result<()> {
    let channels = usize::from(chunk.format.channels());
    if channels > 2 {
        bail!("Multichannel playback requires an explicit channel mapper")
    }
    buffered.extend(
        chunk
            .samples
            .chunks_exact(channels)
            .map(|frame| [frame[0], frame.get(1).copied().unwrap_or(frame[0])]),
    );
    Ok(())
}

fn scale_frame(frame: u64, from_rate: u32, to_rate: u32) -> u64 {
    (frame as f64 * f64::from(to_rate) / f64::from(from_rate)).round() as u64
}

fn queue_tracks(
    tracks: &[Track],
    rate: u32,
    current: Option<(usize, u64)>,
) -> Vec<QueueTrack<usize>> {
    tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let declared = (track.duration_seconds.max(0.0) * f64::from(rate)).round() as u64;
            let total_frames = current
                .filter(|(current_index, _)| *current_index == index)
                .map_or(declared, |(_, frame)| declared.max(frame));
            QueueTrack::new(index, total_frames)
        })
        .collect()
}

fn restore_audible_queue(
    tracks: &[Track],
    shared: &Shared,
    rate: u32,
    generation: u64,
) -> Result<QueueState<usize>> {
    let index = shared.track.load(Ordering::Relaxed);
    let position = shared.position.load(Ordering::Relaxed);
    let current = (index < tracks.len()).then_some(index);
    Ok(QueueState::restore(
        queue_tracks(tracks, rate, current.map(|value| (value, position))),
        current,
        current.map_or(0, |_| position),
        Generation(generation),
    )?)
}

/// Product adapter over the shared bounded resampler; same-rate PCM bypasses it.
struct Converted {
    reader: AudioReader,
    resampler: Option<SincResampler>,
    ready: VecDeque<[f32; 2]>,
    input_position: u64,
    eof: bool,
}
impl Converted {
    fn new(
        track: &Track,
        output_frame: u64,
        rate: u32,
        resolver: Option<&dyn MediaResolver>,
    ) -> Result<Self> {
        let reader = AudioReader::open(track, output_frame, rate, resolver)?;
        let resampler = if reader.rate == rate {
            None
        } else {
            Some(SincResampler::new(
                PcmFormat::new(reader.rate, 2)?,
                rate,
                1_024,
            )?)
        };
        Ok(Self {
            reader,
            resampler,
            ready: VecDeque::new(),
            input_position: 0,
            eof: false,
        })
    }
    fn next(&mut self) -> Result<Option<[f32; 2]>> {
        if self.resampler.is_none() {
            return self.reader.next();
        }
        loop {
            if let Some(f) = self.ready.pop_front() {
                return Ok(Some(f));
            }
            // Drain output before admitting more input. Upsampling can leave
            // more than one output chunk ready for each input chunk.
            if let Some(chunk) = self.resampler.as_mut().unwrap().read_frames(1_024)? {
                for frame in chunk.samples.chunks_exact(2) {
                    self.ready.push_back([frame[0], frame[1]]);
                }
                continue;
            }
            if self.eof {
                return Ok(None);
            }
            let mut samples = Vec::with_capacity(2_048);
            for _ in 0..1_024 {
                match self.reader.next()? {
                    Some(frame) => samples.extend(frame),
                    None => {
                        self.eof = true;
                        break;
                    }
                }
            }
            if !samples.is_empty() {
                let count = samples.len() / 2;
                self.resampler.as_mut().unwrap().push(PcmChunk::new(
                    PcmFormat::new(self.reader.rate, 2)?,
                    FramePosition(self.input_position),
                    samples,
                )?)?;
                self.input_position += count as u64;
            }
            if self.eof {
                self.resampler.as_mut().unwrap().finish()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn fixture(rate: u32, frames: u32) -> (tempfile::TempDir, Track) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wav");
        let mut w = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 2,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for i in 0..frames {
            let s = (i % 1000) as i16;
            w.write_sample(s).unwrap();
            w.write_sample(-s).unwrap();
        }
        w.finalize().unwrap();
        let track = Track {
            scan_status: None,
            blocked_reason: None,
            availability: Default::default(),
            source: None,
            id: amp_core::ItemId {
                library: "test".into(),
                provider: "file".into(),
                item: "1".into(),
            },
            title: "test".into(),
            artist: "test".into(),
            number: 1,
            disc: 1,
            duration_seconds: frames as f64 / rate as f64,
            access: MediaAccess::File {
                locator: path.to_string_lossy().into_owned(),
            },
        };
        (dir, track)
    }
    #[test]
    fn pcm_is_exact_and_seek_starts_at_requested_sample() {
        let (_dir, t) = fixture(44100, 44100);
        let mut r = Converted::new(&t, 0, 44100, None).unwrap();
        for i in 0..44100 {
            let f = r.next().unwrap().unwrap();
            assert_eq!(f[0], (i % 1000) as f32 / 32768.0);
            assert_eq!(f[1], -f[0]);
        }
        assert!(r.next().unwrap().is_none());
        let mut r = Converted::new(&t, 22_050, 44100, None).unwrap();
        assert_eq!(r.next().unwrap().unwrap()[0], 50.0 / 32768.0);
    }
    #[test]
    fn resampling_has_exact_duration_even_for_partial_chunks() {
        for frames in [1, 100, 1024, 4410, 176400] {
            let (_dir, t) = fixture(44100, frames);
            let mut r = Converted::new(&t, 0, 48000, None).unwrap();
            let mut count = 0;
            while r.next().unwrap().is_some() {
                count += 1;
                assert!(count <= 192000);
            }
            assert_eq!(count, (frames as f64 * 48000.0 / 44100.0).round() as u32);
        }
    }
    #[test]
    fn cue_track_boundaries_and_sector_seek_are_sample_exact() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("disc.bin");
        let mut bytes = Vec::new();
        for i in 0..1176i16 {
            bytes.extend_from_slice(&i.to_le_bytes());
            bytes.extend_from_slice(&(-i).to_le_bytes());
        }
        std::fs::write(bin, bytes).unwrap();
        let cue = dir.path().join("disc.cue");
        std::fs::write(&cue,"FILE \"disc.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 00:00:01\n").unwrap();
        let (_other, mut track) = fixture(44100, 1);
        track.access = MediaAccess::DiscTrack {
            cue: cue.to_string_lossy().into_owned(),
            number: 2,
        };
        let mut reader = Converted::new(&track, 0, 44100, None).unwrap();
        for i in 588..1176 {
            assert_eq!(reader.next().unwrap().unwrap()[0], i as f32 / 32768.0);
        }
        assert!(reader.next().unwrap().is_none());
        let mut reader = Converted::new(&track, 100, 44100, None).unwrap();
        assert_eq!(reader.next().unwrap().unwrap()[0], 688.0 / 32768.0);
    }
    #[test]
    fn redumper_matches_bin_cue_and_repeated_sample_seeks() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("disc.cue");
        let origin = 45150u64 * 588;
        let mut scram = File::create(anchor.with_extension("scram")).unwrap();
        scram.set_len((origin + 1176) * 4).unwrap();
        scram.seek(SeekFrom::Start(origin * 4)).unwrap();
        let mut bytes = Vec::new();
        for i in 0..1176i16 {
            bytes.extend(i.to_le_bytes());
            bytes.extend((-i).to_le_bytes());
        }
        scram.write_all(&bytes).unwrap();
        std::fs::write(anchor.with_extension("bin"), bytes).unwrap();
        let mut state = File::create(anchor.with_extension("state")).unwrap();
        state.set_len(origin + 1176).unwrap();
        state.seek(SeekFrom::Start(origin)).unwrap();
        state.write_all(&[4; 1176]).unwrap();
        File::create(anchor.with_extension("subcode"))
            .unwrap()
            .set_len((45150 + 2) * 96)
            .unwrap();
        let mut toc = vec![0, 26, 1, 2];
        for (n, lba) in [(1, 0i32), (2, 1), (0xaa, 2)] {
            toc.extend([0, 0x10, n, 0]);
            toc.extend(lba.to_be_bytes());
        }
        std::fs::write(anchor.with_extension("toc"), toc).unwrap();
        std::fs::write(&anchor, "FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n INDEX 01 00:00:00\n TRACK 02 AUDIO\n INDEX 01 00:00:01\n").unwrap();
        let (_other, mut track) = fixture(44100, 588);
        for number in [1, 2, 1, 2] {
            for offset in [0, 1, 100, 587, 588] {
                track.access = MediaAccess::RedumperTrack {
                    anchor: anchor.to_string_lossy().into_owned(),
                    number,
                };
                let mut raw = Converted::new(&track, offset, 44100, None).unwrap();
                track.access = MediaAccess::DiscTrack {
                    cue: anchor.to_string_lossy().into_owned(),
                    number,
                };
                let mut bin = Converted::new(&track, offset, 44100, None).unwrap();
                for _ in offset..588 {
                    assert_eq!(raw.next().unwrap(), bin.next().unwrap());
                }
                assert!(raw.next().unwrap().is_none());
                assert!(bin.next().unwrap().is_none());
            }
        }
        state.seek(SeekFrom::Start(origin)).unwrap();
        state.write_all(&[1]).unwrap();
        track.access = MediaAccess::RedumperTrack {
            anchor: anchor.to_string_lossy().into_owned(),
            number: 1,
        };
        assert!(
            Converted::new(&track, 0, 44100, None)
                .unwrap()
                .next()
                .is_err()
        );
    }
    struct DecodedFixture {
        position: u64,
    }
    impl PcmReader for DecodedFixture {
        fn format(&self) -> PcmFormat {
            PcmFormat::new(44100, 1).unwrap()
        }
        fn total_frames(&self) -> Option<u64> {
            Some(100)
        }
        fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>> {
            if self.position >= 100 {
                return Ok(None);
            }
            let count = max.min((100 - self.position) as usize);
            let chunk = PcmChunk::new(
                self.format(),
                FramePosition(self.position),
                (0..count)
                    .map(|n| (self.position + n as u64) as f32 / 100.0)
                    .collect(),
            )?;
            self.position += count as u64;
            Ok(Some(chunk))
        }
        fn seek_exact(&mut self, frame: FramePosition) -> Result<()> {
            self.position = frame.0;
            Ok(())
        }
    }
    struct DecodedResolver;
    impl MediaResolver for DecodedResolver {
        fn resolve(&self, _: &Track) -> Result<ByteSource> {
            bail!("Decoded only")
        }
        fn open(&self, _: &Track) -> Result<MediaInput> {
            Ok(MediaInput::Decoded(Box::new(DecodedFixture {
                position: 0,
            })))
        }
        fn cancel(&self) {}
    }
    #[test]
    fn registered_decoded_input_uses_shared_seek_and_channel_conversion() {
        let (_dir, mut track) = fixture(44100, 1);
        track.access = MediaAccess::Managed {
            provider: "native".into(),
            item: "opaque".into(),
        };
        let mut registry = ResolverRegistry::default();
        assert!(registry.open(&track).is_err());
        registry
            .register("native", Arc::new(DecodedResolver))
            .unwrap();
        assert!(
            registry
                .register("native", Arc::new(DecodedResolver))
                .is_err()
        );
        let mut reader = AudioReader::open(&track, 17, 44100, Some(&registry)).unwrap();
        assert_eq!(reader.next().unwrap(), Some([0.17, 0.17]));
        for _ in 18..100 {
            assert!(reader.next().unwrap().is_some());
        }
        assert!(reader.next().unwrap().is_none());
    }
}
