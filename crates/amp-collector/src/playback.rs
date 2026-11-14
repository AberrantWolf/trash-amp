use super::*;
use amp_audio::{ByteSource, MediaHint, MediaInput, PcmReader, StreamDecoder, StreamFormat};
use amp_core::Availability;
use reqwest::{StatusCode, Url, header};
use std::{
    future::Future,
    io::{self, Seek, SeekFrom},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackFormat {
    #[serde(alias = "saved_audio")]
    Original,
    #[default]
    #[serde(alias = "flac")]
    HighBandwidth,
    #[serde(alias = "opus")]
    LowBandwidth { bitrate_kbps: u16 },
}

impl Adapter {
    pub fn playback_format(&self) -> Result<PlaybackFormat> {
        Ok(self.get("settings", "playback-format")?.unwrap_or_default())
    }

    pub fn set_playback_format(&self, format: PlaybackFormat) -> Result<()> {
        if let PlaybackFormat::LowBandwidth { bitrate_kbps } = format {
            anyhow::ensure!(
                (6..=256).contains(&bitrate_kbps),
                "Enter an Opus bitrate from 6 to 256 kbps"
            );
        }
        self.put("settings", "playback-format", &format)
    }

    pub(super) fn open_stream(
        &self,
        remote: &RemoteTrack,
        format: PlaybackFormat,
        position: Duration,
    ) -> Result<Option<MediaInput>> {
        let saved = remote.audio.as_ref().is_some_and(|audio| {
            matches!(
                audio.state,
                api::MusicTrackAvailabilityState::Available { .. }
            )
        });
        let client = self.client(remote.key)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let cancellation = self.media_cancellation(remote.key);
        let result = runtime.block_on(cancellable(&cancellation, async {
            let routes = client
                .get(client.api_url("api/v1/playback/routes")?)?
                .send()
                .await?;
            if routes.status() == StatusCode::NOT_FOUND && saved {
                return Ok(None);
            }
            let descriptor = playback_descriptor(routes, remote.key).await?;
            if !descriptor.remote_media_allowed {
                if saved {
                    return Ok(None);
                }
                bail!("This track needs a normal Collector audio connection or a saved copy");
            }

            let mut url = client.api_url(&format!(
                "api/v1/items/{}/music-stream/{}",
                remote.item, remote.source
            ))?;
            let (mode, bitrate) = match format {
                PlaybackFormat::Original => ("original", None),
                PlaybackFormat::HighBandwidth => ("high_bandwidth", None),
                PlaybackFormat::LowBandwidth { bitrate_kbps } => {
                    ("low_bandwidth", Some(bitrate_kbps))
                }
            };
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("mode", mode)
                    .append_pair("track", &remote.position.to_string());
                if let Some(bitrate) = bitrate {
                    query.append_pair("bitrate", &bitrate.to_string());
                }
                // Collector only applies this offset when it encodes a live response.
                if !position.is_zero() && format != PlaybackFormat::Original {
                    query.append_pair(
                        "start_ms",
                        &u32::try_from(position.as_millis())?.to_string(),
                    );
                }
            }
            // Keep the enrolled client's TLS, credential scope and redirect policy.
            // Each body read has its own timeout.
            let response = client
                .get(url.clone())?
                .timeout(Duration::MAX)
                .send()
                .await?;
            // Older servers either lack this endpoint or reject the mode query.
            // Application errors on the current route use Collector's JSON envelope.
            let content_type = header_text(response.headers(), header::CONTENT_TYPE);
            let legacy = (response.status() == StatusCode::NOT_FOUND && content_type.is_none())
                || (response.status() == StatusCode::BAD_REQUEST
                    && content_type.is_some_and(|value| value.starts_with("text/plain")));
            if legacy && saved {
                return Ok(None);
            }
            let response = response.error_for_status()?;
            anyhow::ensure!(
                response.status() == StatusCode::OK,
                "Unexpected Collector playback response"
            );
            Ok(Some((url, response)))
        }));
        if !matches!(result, Ok(Some(_))) {
            cancellation.cancel();
        }
        let Some((url, response)) = result? else {
            return Ok(None);
        };
        let connection = PlaybackConnection {
            runtime,
            cancellation,
        };
        let headers = response.headers();
        let media_type = header_text(headers, header::CONTENT_TYPE)
            .and_then(|value| value.split(';').next())
            .context("Collector did not report the audio format")?;
        let (extension, codec) = match (format, media_type) {
            (PlaybackFormat::Original, "audio/wav") => ("wav", None),
            (PlaybackFormat::Original | PlaybackFormat::HighBandwidth, "audio/mpeg") => {
                ("mp3", None)
            }
            (PlaybackFormat::Original | PlaybackFormat::HighBandwidth, "audio/flac") => {
                ("flac", Some(StreamFormat::Flac))
            }
            (PlaybackFormat::Original | PlaybackFormat::LowBandwidth { .. }, "audio/ogg") => {
                ("opus", Some(StreamFormat::Opus))
            }
            _ => bail!("Collector returned an unexpected playback format"),
        };

        if header_text(headers, header::ACCEPT_RANGES) != Some("bytes") {
            let codec = codec.context("Collector returned audio without byte ranges")?;
            return Ok(Some(MediaInput::Streamed(Box::new(StreamDecoder::open(
                HttpStream::new(connection, response),
                codec,
            )?))));
        }

        let length = header_text(headers, header::CONTENT_LENGTH)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|length| *length > 0)
            .context("Collector did not report the audio length")?;
        let etag = header_text(headers, header::ETAG)
            .filter(|value| value.len() >= 2 && value.starts_with('"') && value.ends_with('"'))
            .context("Collector did not report a strong audio validator")?
            .to_owned();
        let hint = MediaHint::new(Some(length), Some(extension), Some(media_type));
        drop(response);
        let file = HttpFile {
            connection,
            url,
            client,
            etag,
            length,
            position: 0,
            range: None,
        };
        if matches!(codec, Some(StreamFormat::Opus)) {
            // A prepared Opus file starts at zero. Decode from its headers to
            // preserve pre-skip and decoder state before discarding the prefix.
            let mut reader = StreamDecoder::open(file, StreamFormat::Opus)?;
            let mut remaining =
                (position.as_secs_f64() * f64::from(reader.format().sample_rate())).round() as u64;
            while remaining > 0 {
                let chunk = reader
                    .read_frames(remaining.min(4096) as usize)?
                    .context("Playback position is beyond the track")?;
                remaining -= chunk.frame_count();
            }
            return Ok(Some(MediaInput::Streamed(Box::new(reader))));
        }

        Ok(Some(MediaInput::Encoded(ByteSource::new(file, hint))))
    }
}

pub(super) async fn playback_descriptor(
    response: reqwest::Response,
    key: ConnectionKey,
) -> Result<api::LocalPlaybackDescriptor> {
    let mut response = response.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len() + chunk.len() <= 128 * 1024,
            "Oversized playback descriptor"
        );
        bytes.extend_from_slice(&chunk);
    }
    let descriptor: api::LocalPlaybackDescriptor = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        descriptor.server_id == key.server_id,
        "Playback server identity mismatch"
    );
    Ok(descriptor)
}

pub(super) fn online_availability(remote: &RemoteTrack) -> Availability {
    let Some(audio) = remote.audio.as_ref() else {
        return Availability::Unavailable;
    };
    match audio.state {
        _ if audio.can_stream_original => Availability::Available,
        api::MusicTrackAvailabilityState::Available { .. } => Availability::Available,
        api::MusicTrackAvailabilityState::Preparing => Availability::Preparing,
        _ => Availability::Unavailable,
    }
}

struct PlaybackConnection {
    runtime: tokio::runtime::Runtime,
    cancellation: MediaCancellation,
}

impl Drop for PlaybackConnection {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

struct HttpStream {
    connection: PlaybackConnection,
    response: reqwest::Response,
    chunk: Option<io::Cursor<bytes::Bytes>>,
}

impl HttpStream {
    fn new(connection: PlaybackConnection, response: reqwest::Response) -> Self {
        Self {
            connection,
            response,
            chunk: None,
        }
    }
}

impl Read for HttpStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.connection.cancellation.is_cancelled() {
            return Err(io::Error::other("Playback cancelled"));
        }
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(chunk) = &mut self.chunk {
                let count = chunk.read(output)?;
                if count > 0 {
                    return Ok(count);
                }
            }
            let chunk = self
                .connection
                .runtime
                .block_on(cancellable(&self.connection.cancellation, async {
                    Ok(self.response.chunk().await?)
                }))
                .map_err(io::Error::other)?;
            let Some(chunk) = chunk else { return Ok(0) };
            self.chunk = Some(io::Cursor::new(chunk));
        }
    }
}

fn header_text(headers: &header::HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

struct HttpFile {
    connection: PlaybackConnection,
    url: Url,
    client: EnrolledClient,
    etag: String,
    length: u64,
    position: u64,
    range: Option<(u64, io::Cursor<Vec<u8>>)>,
}

impl Read for HttpFile {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.connection.cancellation.is_cancelled() {
            return Err(io::Error::other("Playback cancelled"));
        }
        if output.is_empty() || self.position >= self.length {
            return Ok(0);
        }
        if !self.range.as_ref().is_some_and(|(start, buffer)| {
            (*start..*start + buffer.get_ref().len() as u64).contains(&self.position)
        }) {
            self.fetch_range()?;
        }
        let (start, buffer) = self.range.as_mut().expect("range loaded");
        buffer.set_position(self.position - *start);
        let count = buffer.read(output)?;
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for HttpFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(n) => self.length.checked_add_signed(n),
            SeekFrom::Current(n) => self.position.checked_add_signed(n),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid audio seek"))?;
        self.position = next;
        Ok(next)
    }
}

impl HttpFile {
    fn fetch_range(&mut self) -> io::Result<()> {
        const CHUNK: u64 = 256 * 1024;
        let start = self.position;
        let end = start.saturating_add(CHUNK - 1).min(self.length - 1);
        let expected_range = format!("bytes {start}-{end}/{}", self.length);
        let wanted = (end - start + 1) as usize;
        let bytes = self
            .connection
            .runtime
            .block_on(cancellable(&self.connection.cancellation, async {
                let mut response = self
                    .client
                    .get(self.url.clone())?
                    .header(header::RANGE, format!("bytes={start}-{end}"))
                    .header(header::IF_RANGE, &self.etag)
                    .send()
                    .await?;
                anyhow::ensure!(
                    response.status() == StatusCode::PARTIAL_CONTENT
                        && header_text(response.headers(), header::CONTENT_RANGE)
                            == Some(expected_range.as_str())
                        && header_text(response.headers(), header::ETAG)
                            == Some(self.etag.as_str()),
                    "Collector audio changed or is no longer available (HTTP {})",
                    response.status()
                );
                let mut bytes = Vec::with_capacity(wanted);
                while let Some(chunk) = response.chunk().await? {
                    anyhow::ensure!(chunk.len() <= wanted - bytes.len(), "Oversized audio range");
                    bytes.extend_from_slice(&chunk);
                }
                anyhow::ensure!(bytes.len() == wanted, "Truncated audio range");
                Ok(bytes)
            }))
            .map_err(io::Error::other)?;
        self.range = Some((start, io::Cursor::new(bytes)));
        Ok(())
    }
}

async fn cancellable<T>(
    cancellation: &MediaCancellation,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(30), work) => result.context("Collector playback timed out")?,
        () = async {
            while !cancellation.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        } => bail!("Playback cancelled"),
    }
}
