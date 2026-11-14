//! Decode sequential Collector responses. Seeking opens a fresh HTTP response.
use crate::{FramePosition, PcmChunk, PcmFormat, PcmReader};
use anyhow::{Context, Result, bail};
use std::{
    collections::VecDeque,
    io::{self, Read, Seek, SeekFrom},
    sync::Mutex,
};
use symphonia::core::{
    audio::{Layout, SampleBuffer},
    codecs::{CODEC_TYPE_FLAC, CODEC_TYPE_OPUS, CodecParameters, Decoder, DecoderOptions},
    errors::Error,
    formats::{FormatOptions, FormatReader, Packet},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    probe::Hint,
};

#[derive(Clone, Copy)]
pub enum StreamFormat {
    Flac,
    Opus,
}

struct SequentialSource<R>(Mutex<R>);

impl<R: Read> Read for SequentialSource<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0
            .get_mut()
            .map_err(|_| io::Error::other("Audio stream reader is poisoned"))?
            .read(bytes)
    }
}

impl<R> Seek for SequentialSource<R> {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl<R: Read + Send> MediaSource for SequentialSource<R> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

pub struct StreamDecoder {
    packets: Packets,
    decoder: Box<dyn Decoder>,
    format: PcmFormat,
    position: FramePosition,
    buffered: VecDeque<f32>,
}

enum Packets {
    Flac {
        container: Box<dyn FormatReader>,
        track: u32,
    },
    Opus {
        reader: ogg::PacketReader<Box<dyn junk_libs_audio::AudioReadSeek>>,
        serial: u32,
        pre_skip: u64,
        skip_remaining: u64,
        finished: bool,
    },
}

impl StreamDecoder {
    pub fn open(reader: impl Read + Send + 'static, format: StreamFormat) -> Result<Self> {
        match format {
            StreamFormat::Flac => Self::open_flac(reader),
            StreamFormat::Opus => Self::open_opus(reader),
        }
    }

    fn open_flac(reader: impl Read + Send + 'static) -> Result<Self> {
        let mut hint = Hint::new();
        hint.with_extension("flac");
        let stream = MediaSourceStream::new(
            Box::new(SequentialSource(Mutex::new(reader))),
            Default::default(),
        );
        let container = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions {
                    limit_metadata_bytes: symphonia::core::meta::Limit::Maximum(32 * 1024 * 1024),
                    limit_visual_bytes: symphonia::core::meta::Limit::Maximum(20 * 1024 * 1024),
                },
            )?
            .format;
        let track = container
            .default_track()
            .context("Stream has no audio track")?;
        let params = &track.codec_params;
        let channels = params
            .channels
            .context("Stream has no channel information")?
            .count();
        anyhow::ensure!(matches!(channels, 1 | 2), "Stream must be mono or stereo");
        let pcm_format = PcmFormat::new(
            params.sample_rate.context("Stream has no sample rate")?,
            channels as u16,
        )?;
        anyhow::ensure!(
            params.codec == CODEC_TYPE_FLAC,
            "Collector returned audio in a different format"
        );
        let decoder = symphonia::default::get_codecs().make(params, &DecoderOptions::default())?;
        let track = track.id;
        Ok(Self {
            packets: Packets::Flac { container, track },
            decoder,
            format: pcm_format,
            position: FramePosition(0),
            buffered: VecDeque::new(),
        })
    }

    fn open_opus(reader: impl Read + Send + 'static) -> Result<Self> {
        let source: Box<dyn junk_libs_audio::AudioReadSeek> =
            Box::new(SequentialSource(Mutex::new(reader)));
        let mut reader = ogg::PacketReader::new(source);
        let header = reader.read_packet_expected()?;
        anyhow::ensure!(
            header.first_in_stream()
                && header.data.len() >= 19
                && header.data.starts_with(b"OpusHead")
                && header.data[8] >> 4 == 0
                && header.data[18] == 0,
            "Unsupported Ogg Opus header"
        );
        let channels = header.data[9];
        let layout = match channels {
            1 => Layout::Mono,
            2 => Layout::Stereo,
            _ => bail!("Stream must be mono or stereo"),
        };
        let serial = header.stream_serial();
        let tags = reader.read_packet_expected()?;
        anyhow::ensure!(
            tags.stream_serial() == serial && tags.data.starts_with(b"OpusTags"),
            "Missing Ogg Opus tags"
        );
        let pre_skip = u64::from(u16::from_le_bytes([header.data[10], header.data[11]]));
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_OPUS)
            .with_sample_rate(48_000)
            .with_channels(layout.into_channels());
        let decoder =
            symphonia_adapter_libopus::OpusDecoder::try_new(&params, &DecoderOptions::default())?;
        Ok(Self {
            packets: Packets::Opus {
                reader,
                serial,
                pre_skip,
                skip_remaining: pre_skip,
                finished: false,
            },
            decoder: Box::new(decoder),
            format: PcmFormat::new(48_000, u16::from(channels))?,
            position: FramePosition(0),
            buffered: VecDeque::new(),
        })
    }

    fn decode_packet(&mut self) -> Result<bool> {
        let (packet, skip, end) = match &mut self.packets {
            Packets::Flac { container, track } => {
                let packet = loop {
                    match container.next_packet() {
                        Ok(packet) if packet.track_id() == *track => break packet,
                        Ok(_) => {}
                        Err(Error::IoError(error))
                            if error.kind() == io::ErrorKind::UnexpectedEof =>
                        {
                            return Ok(false);
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                (packet, None, None)
            }
            Packets::Opus {
                reader,
                serial,
                pre_skip,
                skip_remaining,
                finished,
            } => {
                if *finished {
                    return Ok(false);
                }
                let packet = reader.read_packet_expected()?;
                anyhow::ensure!(
                    packet.stream_serial() == *serial,
                    "Unexpected Ogg Opus stream"
                );
                *finished = packet.last_in_stream();
                // Opus granule positions count decoded frames including the encoder's pre-skip.
                let end = finished.then(|| packet.absgp_page().saturating_sub(*pre_skip));
                (
                    Packet::new_from_boxed_slice(*serial, 0, 0, packet.data.into_boxed_slice()),
                    Some(skip_remaining),
                    end,
                )
            }
        };
        let decoded = self.decoder.decode(&packet)?;
        let channels = usize::from(self.format.channels());
        anyhow::ensure!(
            decoded.spec().rate == self.format.sample_rate()
                && decoded.spec().channels.count() == channels,
            "Audio format changed within the stream"
        );
        let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        samples.copy_interleaved_ref(decoded);
        let mut samples = samples.samples();
        if let Some(skip) = skip {
            let skipped = (*skip).min((samples.len() / channels) as u64);
            *skip -= skipped;
            samples = &samples[skipped as usize * channels..];
        }
        if let Some(end) = end {
            let buffered_end = self.position.0 + (self.buffered.len() / channels) as u64;
            let remaining = end
                .saturating_sub(buffered_end)
                .min((samples.len() / channels) as u64);
            samples = &samples[..remaining as usize * channels];
        }
        self.buffered.extend(samples);
        Ok(true)
    }
}

impl PcmReader for StreamDecoder {
    fn format(&self) -> PcmFormat {
        self.format
    }

    fn total_frames(&self) -> Option<u64> {
        None
    }

    fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>> {
        let channels = usize::from(self.format.channels());
        let wanted = max * channels;
        while self.buffered.len() < wanted && self.decode_packet()? {}
        let count = self.buffered.len().min(wanted);
        if count == 0 {
            return Ok(None);
        }
        let chunk = PcmChunk::new(
            self.format,
            self.position,
            self.buffered.drain(..count).collect(),
        )?;
        self.position.0 += (count / channels) as u64;
        Ok(Some(chunk))
    }

    fn seek_exact(&mut self, _: FramePosition) -> Result<()> {
        bail!("A sequential stream must be reopened to seek")
    }
}
