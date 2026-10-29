//! Output devices and stream callbacks. All device operations run on the audio worker.
use super::{ClockGuard, Frame, Shared, processing::ProcessingChain};
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Default)]
pub(super) struct Health {
    pub failed: AtomicBool,
    pub callbacks: AtomicU64,
}

pub(super) struct Output<S> {
    // Drop the stream before releasing its producer or health state.
    pub stream: S,
    pub running: bool,
    pub producer: rtrb::Producer<Frame>,
    pub rate: u32,
    pub key: String,
    pub health: Arc<Health>,
}

pub(super) trait Backend: Send + 'static {
    type Stream;
    /// Open stopped so the worker can restore buffered PCM before callbacks start.
    fn open(&mut self, shared: Arc<Shared>) -> Result<Output<Self::Stream>>;
    fn default_key(&mut self) -> Result<String>;
    fn set_playing(&mut self, stream: &mut Self::Stream, playing: bool) -> Result<()>;
    fn release_when_paused(&self) -> bool {
        false
    }
}

pub(super) struct CpalBackend;

pub(super) fn buffer_capacity(rate: u32, retain_on_pause: bool) -> usize {
    if retain_on_pause {
        rate as usize * 15
    } else {
        rate as usize / 2
    }
}

pub(super) struct CpalStream {
    // The audio unit must stop before the session is released.
    stream: Option<cpal::Stream>,
    #[cfg(target_os = "ios")]
    session: super::ios_session::AudioSession,
}

fn default_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig, String)> {
    let device = cpal::default_host()
        .default_output_device()
        .context("No audio output device")?;
    let config = device.default_output_config()?;
    let key = format!(
        "{}:{}:{}:{:?}",
        device.id()?,
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    );
    Ok((device, config, key))
}

impl Backend for CpalBackend {
    type Stream = CpalStream;

    fn release_when_paused(&self) -> bool {
        // Remove CPAL's interruption observers too: they can reactivate the
        // shared iOS session when an interruption ends, even for a paused unit.
        cfg!(target_os = "ios")
    }

    fn set_playing(&mut self, stream: &mut Self::Stream, playing: bool) -> Result<()> {
        if playing {
            #[cfg(target_os = "ios")]
            stream.session.activate()?;
            stream
                .stream
                .as_ref()
                .context("Audio output retired")?
                .play()?;
        } else {
            if let Err(error) = stream
                .stream
                .as_ref()
                .context("Audio output retired")?
                .pause()
            {
                // Some desktop devices (e.g. ALSA) only support software pause.
                // Keep their existing silent-callback behavior without a retry loop.
                if cfg!(target_os = "ios") || error.kind() != cpal::ErrorKind::UnsupportedOperation
                {
                    return Err(error.into());
                }
            }
            #[cfg(target_os = "ios")]
            {
                // Remove the audio unit and interruption observers before
                // deactivation, and report deactivation errors to the worker.
                drop(stream.stream.take());
                stream.session.deactivate()?;
            }
        }
        Ok(())
    }

    fn default_key(&mut self) -> Result<String> {
        Ok(default_device()?.2)
    }

    fn open(&mut self, shared: Arc<Shared>) -> Result<Output<Self::Stream>> {
        #[cfg(target_os = "ios")]
        let session = super::ios_session::AudioSession::new(shared.clone())?;
        let (device, supported, key) = default_device()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let health = Arc::new(Health::default());
        let capacity = if shared.buffer_seconds == 0 {
            buffer_capacity(config.sample_rate, self.release_when_paused())
        } else {
            config.sample_rate as usize * shared.buffer_seconds as usize
        };
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let stream = match format {
            cpal::SampleFormat::F32 => {
                stream::<f32>(&device, &config, consumer, shared, health.clone())?
            }
            cpal::SampleFormat::I16 => {
                stream::<i16>(&device, &config, consumer, shared, health.clone())?
            }
            cpal::SampleFormat::I32 => {
                stream::<i32>(&device, &config, consumer, shared, health.clone())?
            }
            cpal::SampleFormat::U16 => {
                stream::<u16>(&device, &config, consumer, shared, health.clone())?
            }
            other => bail!("Unsupported output sample format: {other}"),
        };
        // The worker restores the generation and preloads retained audio before
        // starting callbacks. No network access is needed to play that audio.
        Ok(Output {
            stream: CpalStream {
                stream: Some(stream),
                #[cfg(target_os = "ios")]
                session,
            },
            running: false,
            producer,
            rate: config.sample_rate,
            key,
            health,
        })
    }
}

fn stream<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: rtrb::Consumer<Frame>,
    shared: Arc<Shared>,
    health: Arc<Health>,
) -> Result<cpal::Stream> {
    let channels = usize::from(config.channels);
    let errors = health.clone();
    let mut processing = ProcessingChain::new(config.sample_rate, channels);
    Ok(device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            render(
                data,
                channels,
                &mut consumer,
                &shared,
                &health,
                &mut processing,
            )
        },
        // A retired stream can only mark its own health, never pause a replacement.
        move |_| {
            errors.failed.store(true, Ordering::Release);
        },
        None,
    )?)
}

pub(super) fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    consumer: &mut rtrb::Consumer<Frame>,
    shared: &Shared,
    health: &Health,
    processing: &mut ProcessingChain,
) {
    health.callbacks.fetch_add(1, Ordering::Relaxed);
    let Some(_clock) = ClockGuard::try_lock(&shared.clock_writer) else {
        data.fill(T::from_sample(0.0));
        return;
    };
    let generation = shared.generation.load(Ordering::Acquire);
    processing.begin_callback(generation, shared.processing.load(Ordering::Acquire));
    while consumer
        .peek()
        .is_ok_and(|frame| frame.generation != generation)
    {
        let _ = consumer.pop();
    }
    let mut peak = 0.0f32;
    let mut starved = false;
    for output in data.chunks_mut(channels) {
        let frame = if !health.failed.load(Ordering::Acquire)
            && !shared.shutdown.load(Ordering::Relaxed)
            && shared.playing.load(Ordering::Relaxed)
        {
            match consumer.pop() {
                Ok(frame) if frame.generation == generation => {
                    shared.track.store(frame.track, Ordering::Relaxed);
                    // Store the next frame, so recovery never repeats the last delivered sample.
                    shared.position.store(frame.position + 1, Ordering::Relaxed);
                    Some(frame)
                }
                _ => {
                    starved = true;
                    processing.reset();
                    None
                }
            }
        } else {
            None
        };
        let samples = frame
            .map(|frame| processing.process_frame(frame.samples))
            .unwrap_or_default();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processing;
    use amp_core::{AudioProcessingSettings, CrossfeedSettings};
    use cpal::Sample;

    #[test]
    fn render_preserves_gapless_history_meters_processed_audio_and_silences_pauses_and_underruns() {
        let settings = AudioProcessingSettings {
            crossfeed: CrossfeedSettings {
                enabled: true,
                ..Default::default()
            },
        };
        let shared = Shared::default();
        shared.playing.store(true, Ordering::Relaxed);
        shared.generation.store(1, Ordering::Release);
        shared
            .processing
            .store(processing::pack(settings), Ordering::Release);
        let health = Health::default();
        let (mut producer, mut consumer) = rtrb::RingBuffer::new(2000);
        let mut chain = ProcessingChain::new(48000, 2);
        let mut reference = ProcessingChain::new(48000, 2);
        reference.begin_callback(1, processing::pack(settings));
        for i in 0..1500 {
            producer
                .push(Frame {
                    samples: [0.5, 0.0],
                    generation: 1,
                    track: usize::from(i >= 700),
                    position: if i >= 700 { i - 700 } else { i },
                })
                .unwrap();
        }
        let mut first = vec![0.0; 1000];
        render(&mut first, 2, &mut consumer, &shared, &health, &mut chain);
        let expected: Vec<_> = (0..500)
            .flat_map(|_| reference.process_frame([0.5, 0.0]))
            .collect();
        assert_eq!(first, expected);
        let peak = first
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        assert_eq!(f32::from_bits(shared.level.load(Ordering::Relaxed)), peak);
        shared.playing.store(false, Ordering::Relaxed);
        let mut paused = [1.0; 128];
        render(&mut paused, 2, &mut consumer, &shared, &health, &mut chain);
        assert_eq!(paused, [0.0; 128]);
        shared.playing.store(true, Ordering::Relaxed);
        let mut rest = vec![0.0; 2200];
        render(&mut rest, 2, &mut consumer, &shared, &health, &mut chain);
        let expected: Vec<_> = (0..1000)
            .flat_map(|_| reference.process_frame([0.5, 0.0]))
            .collect();
        assert_eq!(&rest[..2000], expected);
        assert!(rest[2000..].iter().all(|s| *s == 0.0));
        assert_eq!(shared.track.load(Ordering::Relaxed), 1);
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
        producer
            .push(Frame {
                samples: [0.0, 0.0],
                generation: 1,
                ..Default::default()
            })
            .unwrap();
        let mut after_gap = [1.0; 2];
        render(
            &mut after_gap,
            2,
            &mut consumer,
            &shared,
            &health,
            &mut chain,
        );
        assert_eq!(after_gap, [0.0, 0.0]);
    }

    #[test]
    fn mono_bypasses_crossfeed_and_integer_surround_output_maps_after_processing() {
        let shared = Shared::default();
        shared.playing.store(true, Ordering::Relaxed);
        shared.processing.store(
            processing::pack(AudioProcessingSettings {
                crossfeed: CrossfeedSettings {
                    enabled: true,
                    ..Default::default()
                },
            }),
            Ordering::Release,
        );
        let health = Health::default();
        let (mut producer, mut consumer) = rtrb::RingBuffer::new(1);
        producer
            .push(Frame {
                samples: [0.5, 0.0],
                ..Default::default()
            })
            .unwrap();
        let mut mono = [0.0];
        render(
            &mut mono,
            1,
            &mut consumer,
            &shared,
            &health,
            &mut ProcessingChain::new(48000, 1),
        );
        assert_eq!(mono, [0.25]);
        producer
            .push(Frame {
                samples: [0.5, 0.0],
                ..Default::default()
            })
            .unwrap();
        let mut output = [0_i16; 4];
        render(
            &mut output,
            4,
            &mut consumer,
            &shared,
            &health,
            &mut ProcessingChain::new(48000, 4),
        );
        let mut reference = ProcessingChain::new(48000, 4);
        reference.begin_callback(0, shared.processing.load(Ordering::Acquire));
        let samples = reference.process_frame([0.5, 0.0]);
        assert_eq!(
            output,
            [
                i16::from_sample(samples[0]),
                i16::from_sample(samples[1]),
                0,
                0
            ]
        );
    }
}
