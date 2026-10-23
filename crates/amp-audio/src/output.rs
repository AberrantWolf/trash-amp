//! Output devices and stream callbacks. All device operations run on the audio worker.
use super::{Frame, Shared};
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
    pub _stream: S,
    pub producer: rtrb::Producer<Frame>,
    pub rate: u32,
    pub key: String,
    pub health: Arc<Health>,
}

pub(super) trait Backend: Send + 'static {
    type Stream;
    fn open(&mut self, shared: Arc<Shared>) -> Result<Output<Self::Stream>>;
    fn default_key(&mut self) -> Result<String>;
}

pub(super) struct CpalBackend;

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
    type Stream = cpal::Stream;

    fn default_key(&mut self) -> Result<String> {
        Ok(default_device()?.2)
    }

    fn open(&mut self, shared: Arc<Shared>) -> Result<Output<Self::Stream>> {
        let (device, supported, key) = default_device()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let health = Arc::new(Health::default());
        let (producer, consumer) = rtrb::RingBuffer::new(config.sample_rate as usize / 2);
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
        stream.play()?;
        Ok(Output {
            _stream: stream,
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
    Ok(device.build_output_stream(
        *config,
        move |data: &mut [T], _| render(data, channels, &mut consumer, &shared, &health),
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
) {
    health.callbacks.fetch_add(1, Ordering::Relaxed);
    let generation = shared.generation.load(Ordering::Acquire);
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
                    None
                }
            }
        } else {
            None
        };
        let samples = frame.map(|frame| frame.samples).unwrap_or_default();
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
