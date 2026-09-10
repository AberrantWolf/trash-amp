//! Streaming PCM pipeline. Only bounded ring-buffer reads and atomics run in the device callback.
use amp_core::{MediaAccess, Track};
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use junk_libs_audio::{
    ByteSource, CuePcmDecoder, FramePosition, MediaHint, PcmChunk, PcmFormat, RedumperPcmDecoder,
    SincResampler, WavDecoder,
};
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
    time::Duration,
};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{Decoder, DecoderOptions},
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
    units::Time,
};

#[derive(Clone, Copy, Default)]
struct Frame {
    samples: [f32; 2],
    generation: u64,
    track: usize,
    position: u64,
}
#[derive(Default)]
struct Shared {
    playing: AtomicBool,
    generation: AtomicU64,
    track: AtomicUsize,
    position: AtomicU64,
    level: AtomicU32,
    underruns: AtomicU64,
    shutdown: AtomicBool,
    device_failed: AtomicBool,
}
pub struct Snapshot {
    pub playing: bool,
    pub track: usize,
    pub seconds: f64,
    pub level: f32,
    pub underruns: u64,
}
enum Command {
    Replace(Vec<Track>, usize),
    Seek(u64),
    Next,
    Previous,
    Append(Vec<Track>),
    Truncate(usize),
}
pub struct Player {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
    errors: mpsc::Receiver<String>,
    _stream: cpal::Stream,
    rate: u32,
}
impl Player {
    pub fn new() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("No audio output device")?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate;
        let shared = Arc::new(Shared::default());
        let (producer, consumer) = rtrb::RingBuffer::<Frame>::new(rate as usize / 2);
        let stream = match format {
            cpal::SampleFormat::F32 => output::<f32>(&device, &config, consumer, shared.clone())?,
            cpal::SampleFormat::I16 => output::<i16>(&device, &config, consumer, shared.clone())?,
            cpal::SampleFormat::I32 => output::<i32>(&device, &config, consumer, shared.clone())?,
            cpal::SampleFormat::U16 => output::<u16>(&device, &config, consumer, shared.clone())?,
            other => bail!("Unsupported output sample format: {other}"),
        };
        stream.play()?;
        let (commands, rx) = mpsc::channel();
        let (tx, errors) = mpsc::channel();
        let state = shared.clone();
        thread::Builder::new()
            .name("amp-decode".into())
            .spawn(move || worker(rx, tx, producer, state, rate))?;
        Ok(Self {
            commands,
            shared,
            errors,
            _stream: stream,
            rate,
        })
    }
    pub fn play(&self, tracks: Vec<Track>, start: usize) {
        let _ = self.commands.send(Command::Replace(tracks, start));
    }
    pub fn toggle(&self) {
        self.shared.playing.fetch_xor(true, Ordering::Relaxed);
    }
    pub fn seek(&self, seconds: f64) {
        let frame = if seconds.is_finite() {
            (seconds.max(0.0) * f64::from(self.rate)).round() as u64
        } else {
            0
        };
        let _ = self.commands.send(Command::Seek(frame));
    }
    pub fn next(&self) {
        let _ = self.commands.send(Command::Next);
    }
    pub fn previous(&self) {
        // Standard transport behavior: restart after the first few seconds.
        if self.snapshot().seconds > 3.0 {
            self.seek(0.0);
        } else {
            let _ = self.commands.send(Command::Previous);
        }
    }
    pub fn append(&self, tracks: Vec<Track>) {
        let _ = self.commands.send(Command::Append(tracks));
    }
    pub fn truncate(&self, len: usize) {
        let _ = self.commands.send(Command::Truncate(len));
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            playing: self.shared.playing.load(Ordering::Relaxed),
            track: self.shared.track.load(Ordering::Relaxed),
            seconds: self.shared.position.load(Ordering::Relaxed) as f64 / self.rate as f64,
            level: f32::from_bits(self.shared.level.load(Ordering::Relaxed)),
            underruns: self.shared.underruns.load(Ordering::Relaxed),
        }
    }
    pub fn error(&self) -> Option<String> {
        if self.shared.device_failed.swap(false, Ordering::Relaxed) {
            Some("Audio device disconnected or failed. Restart the player to reopen output.".into())
        } else {
            self.errors.try_recv().ok()
        }
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
    }
}
fn output<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: rtrb::Consumer<Frame>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let errors = shared.clone();
    Ok(device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let generation = shared.generation.load(Ordering::Acquire);
            // Discard only the finite old ring; seeking cannot replay stale samples, even while paused.
            while consumer.peek().is_ok_and(|f| f.generation != generation) {
                let _ = consumer.pop();
            }
            let mut peak = 0.0f32;
            let mut starved = false;
            for output in data.chunks_mut(channels) {
                let frame = if shared.playing.load(Ordering::Relaxed) {
                    match consumer.pop() {
                        Ok(f) if f.generation == generation => {
                            shared.track.store(f.track, Ordering::Relaxed);
                            shared.position.store(f.position, Ordering::Relaxed);
                            Some(f)
                        }
                        _ => {
                            starved = true;
                            None
                        }
                    }
                } else {
                    None
                };
                let samples = frame.map(|f| f.samples).unwrap_or_default();
                peak = peak.max(samples[0].abs()).max(samples[1].abs());
                for (channel, sample) in output.iter_mut().enumerate() {
                    let value = if channels == 1 {
                        (samples[0] + samples[1]) * 0.5
                    } else {
                        samples.get(channel).copied().unwrap_or(0.0)
                    };
                    *sample = T::from_sample(value);
                }
            }
            shared
                .level
                .store(peak.min(1.0).to_bits(), Ordering::Relaxed);
            if starved {
                shared.underruns.fetch_add(1, Ordering::Relaxed);
            }
        },
        move |_| {
            errors.device_failed.store(true, Ordering::Relaxed);
            errors.playing.store(false, Ordering::Relaxed);
        },
        None,
    )?)
}

struct FileDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
}
enum Source {
    Wav(WavDecoder),
    Compressed(FileDecoder),
    Disc(CuePcmDecoder),
    Raw(RedumperPcmDecoder),
}
struct AudioReader {
    source: Source,
    buffered: VecDeque<[f32; 2]>,
    rate: u32,
}
impl AudioReader {
    fn open(track: &Track, output_frame: u64, output_rate: u32) -> Result<Self> {
        match &track.access {
            MediaAccess::Managed { provider, .. } => {
                bail!("{provider}: playback adapter is not implemented")
            }
            MediaAccess::DiscTrack { cue, number } => {
                let mut reader = CuePcmDecoder::new(
                    CueAudioDisc::open(std::path::Path::new(cue))?.track_reader(*number)?,
                );
                let source_frame = scale_frame(output_frame, output_rate, 44_100);
                reader.seek_exact(FramePosition(source_frame.min(reader.total_frames())))?;
                Ok(Self {
                    source: Source::Disc(reader),
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
                    source: Source::Raw(reader),
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
                        source: Source::Wav(decoder),
                        buffered: VecDeque::new(),
                    });
                }

                let stream =
                    MediaSourceStream::new(Box::new(File::open(locator)?), Default::default());
                let mut hint = Hint::new();
                if let Some(ext) = extension {
                    hint.with_extension(ext);
                }
                let mut format = symphonia::default::get_probe()
                    .format(
                        &hint,
                        stream,
                        &FormatOptions {
                            enable_gapless: true,
                            ..Default::default()
                        },
                        &MetadataOptions::default(),
                    )?
                    .format;
                let track = format.default_track().context("No audio track")?;
                let rate = track
                    .codec_params
                    .sample_rate
                    .context("Unknown sample rate")?;
                let id = track.id;
                let decoder = symphonia::default::get_codecs()
                    .make(&track.codec_params, &DecoderOptions::default())?;
                let seconds = output_frame as f64 / f64::from(output_rate);
                let mut skip = 0;
                if output_frame > 0 {
                    let seeked = format.seek(
                        SeekMode::Accurate,
                        SeekTo::Time {
                            time: Time::from(seconds),
                            track_id: Some(id),
                        },
                    )?;
                    if let Some(tb) = format
                        .tracks()
                        .iter()
                        .find(|t| t.id == id)
                        .and_then(|t| t.codec_params.time_base)
                    {
                        let time =
                            tb.calc_time(seeked.required_ts.saturating_sub(seeked.actual_ts));
                        skip = ((time.seconds as f64 + time.frac) * rate as f64).round() as u64;
                    }
                }
                let mut result = Self {
                    source: Source::Compressed(FileDecoder {
                        format,
                        decoder,
                        track: id,
                    }),
                    buffered: VecDeque::new(),
                    rate,
                };
                for _ in 0..skip {
                    if result.next()?.is_none() {
                        break;
                    }
                }
                Ok(result)
            }
        }
    }
    fn next(&mut self) -> Result<Option<[f32; 2]>> {
        loop {
            if let Some(frame) = self.buffered.pop_front() {
                return Ok(Some(frame));
            }
            match &mut self.source {
                Source::Disc(reader) => {
                    let Some(chunk) = reader.read_frames(588)? else {
                        return Ok(None);
                    };
                    push_stereo(&mut self.buffered, chunk)?;
                }
                Source::Raw(reader) => {
                    let Some(chunk) = reader.read_frames(588)? else {
                        return Ok(None);
                    };
                    push_stereo(&mut self.buffered, chunk.pcm)?;
                }
                Source::Wav(reader) => {
                    let Some(chunk) = reader.read_frames(1_024)? else {
                        return Ok(None);
                    };
                    push_stereo(&mut self.buffered, chunk)?;
                }
                Source::Compressed(reader) => {
                    let packet = match reader.format.next_packet() {
                        Ok(p) => p,
                        Err(symphonia::core::errors::Error::IoError(e))
                            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                        {
                            return Ok(None);
                        }
                        Err(e) => return Err(e.into()),
                    };
                    if packet.track_id() != reader.track {
                        continue;
                    }
                    let audio = reader.decoder.decode(&packet)?;
                    if audio.spec().rate != self.rate {
                        bail!("Midstream sample rate changes are not supported yet")
                    }
                    let channels = audio.spec().channels.count();
                    if channels > 2 {
                        bail!("Multichannel playback requires an explicit channel mapper")
                    }
                    let mut samples =
                        SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
                    samples.copy_interleaved_ref(audio);
                    self.buffered.extend(
                        samples
                            .samples()
                            .chunks(channels)
                            .map(|f| [f[0], f.get(1).copied().unwrap_or(f[0])]),
                    );
                }
            }
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
    fn new(track: &Track, output_frame: u64, rate: u32) -> Result<Self> {
        let reader = AudioReader::open(track, output_frame, rate)?;
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
            if !self.eof {
                let mut samples = Vec::with_capacity(2_048);
                for _ in 0..1_024 {
                    match self.reader.next()? {
                        Some(f) => {
                            samples.extend(f);
                        }
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
            let Some(chunk) = self.resampler.as_mut().unwrap().read_frames(1_024)? else {
                if self.eof {
                    return Ok(None);
                }
                continue;
            };
            for frame in chunk.samples.chunks_exact(2) {
                self.ready.push_back([frame[0], frame[1]]);
            }
        }
    }
}
fn worker(
    rx: mpsc::Receiver<Command>,
    errors: mpsc::Sender<String>,
    mut producer: rtrb::Producer<Frame>,
    shared: Arc<Shared>,
    rate: u32,
) {
    let mut tracks: Vec<Track> = vec![];
    let mut index = 0;
    let mut reader: Option<Converted> = None;
    let mut position = 0;
    let mut generation = 0;
    let mut pending = None;
    let mut exhausted = true;
    while !shared.shutdown.load(Ordering::Relaxed) {
        while let Ok(command) = rx.try_recv() {
            let mut requested_frame = 0;
            let mut reset = true;
            match command {
                Command::Replace(t, start) => {
                    if t.is_empty() {
                        tracks.clear();
                        index = 0;
                        shared.playing.store(false, Ordering::Relaxed);
                        exhausted = true;
                        continue;
                    }
                    let mut policy = QueueState::new();
                    let transition = match policy.replace(queue_tracks(&t, rate, None), start) {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    tracks = t;
                    index = transition
                        .position
                        .expect("replacement has a track")
                        .track_key;
                    shared.playing.store(true, Ordering::Relaxed);
                }
                Command::Seek(s) => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    let transition = match policy.seek(Generation(generation), s) {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    let position = transition.position.expect("seek retains current track");
                    index = position.track_key;
                    requested_frame = position.frame;
                }
                Command::Next => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    index = policy
                        .next_track()
                        .ok()
                        .and_then(|transition| transition.position)
                        .map_or(tracks.len(), |position| position.track_key);
                }
                Command::Previous => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    let transition = match policy.previous_track() {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    index = transition.position.expect("previous has a track").track_key;
                }
                Command::Truncate(len) => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    let transition = match policy.truncate(len) {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(error.to_string());
                            continue;
                        }
                    };
                    tracks.truncate(len);
                    if let Some(position) = transition.position {
                        index = position.track_key;
                        requested_frame = position.frame;
                    } else {
                        index = tracks.len();
                    }
                }
                Command::Append(t) => {
                    tracks.extend(t);
                    reset = false;
                    if exhausted && index < tracks.len() {
                        exhausted = false;
                    }
                }
            }
            if reset {
                generation += 1;
                shared.generation.store(generation, Ordering::Release);
                shared.track.store(index, Ordering::Relaxed);
                position = requested_frame;
                shared.position.store(position, Ordering::Relaxed);
                pending = None;
                reader = None;
                exhausted = false;
                if let Some(track) = tracks.get(index) {
                    match Converted::new(track, requested_frame, rate) {
                        Ok(r) => reader = Some(r),
                        Err(e) => {
                            let _ = errors.send(format!("{}: {e:#}", track.title));
                            shared.playing.store(false, Ordering::Relaxed);
                            exhausted = true;
                        }
                    }
                }
            }
        }
        if !exhausted && reader.is_none() && pending.is_none() {
            if let Some(track) = tracks.get(index) {
                match Converted::new(track, 0, rate) {
                    Ok(r) => reader = Some(r),
                    Err(e) => {
                        let _ = errors.send(format!("{}: {e:#}", track.title));
                        exhausted = true;
                        shared.playing.store(false, Ordering::Relaxed);
                    }
                }
            } else {
                exhausted = true;
            }
        }
        if exhausted || !shared.playing.load(Ordering::Relaxed) || producer.slots() == 0 {
            if exhausted && producer.slots() == producer.buffer().capacity() {
                shared.playing.store(false, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(3));
            continue;
        }
        if pending.is_none()
            && let Some(r) = reader.as_mut()
        {
            match r.next() {
                Ok(Some(samples)) => {
                    pending = Some(Frame {
                        samples,
                        generation,
                        track: index,
                        position,
                    });
                    position += 1;
                }
                Ok(None) => {
                    index += 1;
                    position = 0;
                    reader = None;
                }
                Err(e) => {
                    let _ = errors.send(format!("Playback stopped: {e:#}"));
                    exhausted = true;
                    shared.playing.store(false, Ordering::Relaxed);
                }
            }
        }
        if let Some(frame) = pending.take()
            && let Err(rtrb::PushError::Full(f)) = producer.push(frame)
        {
            pending = Some(f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(rate: u32, frames: u32) -> (tempfile::TempDir, Track) {
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
        let mut r = Converted::new(&t, 0, 44100).unwrap();
        for i in 0..44100 {
            let f = r.next().unwrap().unwrap();
            assert_eq!(f[0], (i % 1000) as f32 / 32768.0);
            assert_eq!(f[1], -f[0]);
        }
        assert!(r.next().unwrap().is_none());
        let mut r = Converted::new(&t, 22_050, 44100).unwrap();
        assert_eq!(r.next().unwrap().unwrap()[0], 50.0 / 32768.0);
    }
    #[test]
    fn resampling_has_exact_duration_even_for_partial_chunks() {
        for frames in [1, 100, 1024, 4410] {
            let (_dir, t) = fixture(44100, frames);
            let mut r = Converted::new(&t, 0, 48000).unwrap();
            let mut count = 0;
            while r.next().unwrap().is_some() {
                count += 1;
                assert!(count < 10000);
            }
            assert_eq!(count, (frames as f64 * 48000.0 / 44100.0).round() as u32);
        }
    }
    #[test]
    fn worker_joins_tracks_without_dropping_or_repeating_frames() {
        let (_dir, t) = fixture(44100, 1176);
        let (tx, rx) = mpsc::channel();
        let (errors, err_rx) = mpsc::channel();
        let (producer, mut consumer) = rtrb::RingBuffer::new(4096);
        let shared = Arc::new(Shared::default());
        let state = shared.clone();
        let thread = thread::spawn(move || worker(rx, errors, producer, state, 44100));
        tx.send(Command::Replace(vec![t.clone(), t], 0)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut frames = Vec::new();
        while frames.len() < 2352 && std::time::Instant::now() < deadline {
            if let Ok(frame) = consumer.pop() {
                frames.push(frame);
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        shared.shutdown.store(true, Ordering::Relaxed);
        thread.join().unwrap();
        assert!(err_rx.try_recv().is_err());
        assert_eq!(frames.len(), 2352);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.track, i / 1176);
            assert_eq!(f.position, (i % 1176) as u64);
            assert_eq!(f.samples[0], ((i % 1176) % 1000) as f32 / 32768.0);
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
        let mut reader = Converted::new(&track, 0, 44100).unwrap();
        for i in 588..1176 {
            assert_eq!(reader.next().unwrap().unwrap()[0], i as f32 / 32768.0);
        }
        assert!(reader.next().unwrap().is_none());
        let mut reader = Converted::new(&track, 100, 44100).unwrap();
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
                let mut raw = Converted::new(&track, offset, 44100).unwrap();
                track.access = MediaAccess::DiscTrack {
                    cue: anchor.to_string_lossy().into_owned(),
                    number,
                };
                let mut bin = Converted::new(&track, offset, 44100).unwrap();
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
        assert!(Converted::new(&track, 0, 44100).unwrap().next().is_err());
    }
}
