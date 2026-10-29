//! AVFoundation decoding, isolated from MediaPlayer library enumeration.
use super::*;
#[cfg(target_os = "ios")]
use amp_audio::PcmReader;
use objc2::{Encode, Encoding};
use objc2_foundation::{NSMutableDictionary, NSNumber};
use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::atomic::{AtomicBool, Ordering},
};
#[cfg(target_os = "ios")]
use std::{sync::mpsc, thread};
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

// Public CoreMedia C ABI. Objective-C asset reader objects remain thread-confined.
#[repr(C)]
#[derive(Clone, Copy)]
struct Time {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}
unsafe impl Encode for Time {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[
            Encoding::LongLong,
            Encoding::Int,
            Encoding::UInt,
            Encoding::LongLong,
        ],
    );
}
#[repr(C)]
#[derive(Clone, Copy)]
struct TimeRange {
    start: Time,
    duration: Time,
}
unsafe impl Encode for TimeRange {
    const ENCODING: Encoding = Encoding::Struct("?", &[Time::ENCODING, Time::ENCODING]);
}
#[repr(C)]
struct AudioDescription {
    rate: f64,
    format: u32,
    flags: u32,
    bytes_packet: u32,
    frames_packet: u32,
    bytes_frame: u32,
    channels: u32,
    bits: u32,
    reserved: u32,
}
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMAudioFormatDescriptionGetStreamBasicDescription(
        desc: *const c_void,
    ) -> *const AudioDescription;
    fn CMSampleBufferGetDataBuffer(sample: *const c_void) -> *const c_void;
    fn CMSampleBufferGetPresentationTimeStamp(sample: *const c_void) -> Time;
    fn CMSampleBufferGetNumSamples(sample: *const c_void) -> isize;
    fn CMBlockBufferCopyDataBytes(
        buffer: *const c_void,
        offset: usize,
        length: usize,
        dest: *mut c_void,
    ) -> i32;
    fn CMBlockBufferGetDataLength(buffer: *const c_void) -> usize;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}
#[repr(C)]
struct SampleBuffer {
    _private: [u8; 0],
}
unsafe impl objc2::RefEncode for SampleBuffer {
    const ENCODING_REF: Encoding =
        Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}
struct Sample(*const c_void);
impl Drop for Sample {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0);
        }
    }
}

#[cfg(target_os = "ios")]
pub(super) fn open(
    adapter: &Adapter,
    resolve: impl FnOnce() -> Result<Retained<NSURL>> + Send + 'static,
) -> Result<MediaInput> {
    let cancel = adapter.read_cancellation();
    let (commands, rx) = mpsc::sync_channel(1);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let token = cancel.clone();
    thread::Builder::new()
        .name("amp-apple-pcm".into())
        .spawn(move || {
            objc2::rc::autoreleasepool(|_| {
                if token.load(Ordering::Acquire) {
                    return;
                }
                let mut reader =
                    match resolve().and_then(|url| NativeReader::from_url(&url, token.clone())) {
                        Ok(reader) => reader,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                if ready_tx.send(Ok(reader.format)).is_err() {
                    return;
                }
                while let Ok(command) = rx.recv() {
                    if token.load(Ordering::Acquire) {
                        break;
                    }
                    objc2::rc::autoreleasepool(|_| match command {
                        ReaderCommand::Read(max, reply) => {
                            let _ = reply.send(reader.read(max));
                        }
                        ReaderCommand::Seek(frame, reply) => {
                            let _ = reply.send(reader.seek(frame));
                        }
                    });
                }
            });
        })?;
    let format = wait_native(&ready, &cancel)?;
    Ok(MediaInput::Decoded(Box::new(Reader {
        commands,
        cancel,
        format,
    })))
}
#[cfg(target_os = "ios")]
enum ReaderCommand {
    Read(usize, mpsc::SyncSender<Result<Option<amp_audio::PcmChunk>>>),
    Seek(u64, mpsc::SyncSender<Result<()>>),
}
#[cfg(target_os = "ios")]
struct Reader {
    commands: mpsc::SyncSender<ReaderCommand>,
    cancel: Arc<AtomicBool>,
    format: amp_audio::PcmFormat,
}
#[cfg(target_os = "ios")]
impl Drop for Reader {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
#[cfg(target_os = "ios")]
impl PcmReader for Reader {
    fn format(&self) -> amp_audio::PcmFormat {
        self.format
    }
    fn total_frames(&self) -> Option<u64> {
        None
    }
    fn read_frames(&mut self, max: usize) -> Result<Option<amp_audio::PcmChunk>> {
        if max == 0 {
            bail!("A PCM read requires at least one frame");
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands.send(ReaderCommand::Read(max.min(8192), tx))?;
        wait_native(&rx, &self.cancel)
    }
    fn seek_exact(&mut self, frame: amp_audio::FramePosition) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands.send(ReaderCommand::Seek(frame.0, tx))?;
        wait_native(&rx, &self.cancel)
    }
}
struct NativeReader {
    asset: Retained<AnyObject>,
    track: Retained<AnyObject>,
    reader: Retained<AnyObject>,
    output: Retained<AnyObject>,
    format: amp_audio::PcmFormat,
    pending: VecDeque<f32>,
    frame: u64,
    cancel: Arc<AtomicBool>,
}
impl NativeReader {
    fn from_url(url: &NSURL, cancel: Arc<AtomicBool>) -> Result<Self> {
        if cancel.load(Ordering::Acquire) {
            bail!("Apple audio read cancelled");
        }
        unsafe {
            let asset: Retained<AnyObject> =
                msg_send![class(c"AVURLAsset"), URLAssetWithURL: url, options: None::<&AnyObject>];
            let tracks: Retained<NSArray<AnyObject>> =
                msg_send![&*asset, tracksWithMediaType: &*NSString::from_str("soun")];
            let track = tracks
                .firstObject()
                .context("No audio track in Apple asset")?;
            let descriptions: Retained<NSArray<AnyObject>> = msg_send![&*track, formatDescriptions];
            let description = descriptions
                .firstObject()
                .context("Missing Apple audio format")?;
            let basic = CMAudioFormatDescriptionGetStreamBasicDescription(
                Retained::as_ptr(&description).cast(),
            );
            let basic = basic.as_ref().context("Unsupported Apple audio format")?;
            if basic.channels > 2 {
                bail!("Multichannel playback requires an explicit channel mapper");
            }
            let format =
                amp_audio::PcmFormat::new(basic.rate.round() as u32, basic.channels as u16)?;
            let (reader, output) = Self::start(&asset, &track, format, 0)?;
            Ok(Self {
                asset,
                track,
                reader,
                output,
                format,
                pending: VecDeque::new(),
                frame: 0,
                cancel,
            })
        }
    }
    unsafe fn start(
        asset: &AnyObject,
        track: &AnyObject,
        format: amp_audio::PcmFormat,
        frame: u64,
    ) -> Result<(Retained<AnyObject>, Retained<AnyObject>)> {
        unsafe {
            let mut error: Option<Retained<NSError>> = None;
            let allocated: objc2::rc::Allocated<AnyObject> =
                msg_send![class(c"AVAssetReader"), alloc];
            let reader: Option<Retained<AnyObject>> =
                msg_send![allocated, initWithAsset: asset, error: &mut error];
            let reader =
                reader.ok_or_else(|| anyhow::anyhow!("Cannot read Apple asset: {error:?}"))?;
            let settings = NSMutableDictionary::<NSString, AnyObject>::new();
            settings.insert(
                &*NSString::from_str("AVFormatIDKey"),
                &*NSNumber::new_u32(0x6c70636d),
            );
            settings.insert(
                &*NSString::from_str("AVLinearPCMIsFloatKey"),
                &*NSNumber::new_bool(true),
            );
            settings.insert(
                &*NSString::from_str("AVLinearPCMBitDepthKey"),
                &*NSNumber::new_u32(32),
            );
            settings.insert(
                &*NSString::from_str("AVLinearPCMIsNonInterleaved"),
                &*NSNumber::new_bool(false),
            );
            settings.insert(
                &*NSString::from_str("AVLinearPCMIsBigEndianKey"),
                &*NSNumber::new_bool(false),
            );
            settings.insert(
                &*NSString::from_str("AVSampleRateKey"),
                &*NSNumber::new_f64(f64::from(format.sample_rate())),
            );
            settings.insert(
                &*NSString::from_str("AVNumberOfChannelsKey"),
                &*NSNumber::new_u32(u32::from(format.channels())),
            );
            let output: Retained<AnyObject> = msg_send![class(c"AVAssetReaderTrackOutput"), assetReaderTrackOutputWithTrack: track, outputSettings: &*settings];
            let _: () = msg_send![&*reader, addOutput: &*output];
            if frame > 0 {
                let range = TimeRange {
                    start: Time {
                        value: i64::try_from(frame)
                            .context("Seek position exceeds native time range")?,
                        timescale: format.sample_rate() as i32,
                        flags: 1,
                        epoch: 0,
                    },
                    duration: Time {
                        value: 0,
                        timescale: 0,
                        flags: 4,
                        epoch: 0,
                    },
                };
                let _: () = msg_send![&*reader, setTimeRange: range];
            }
            let started: bool = msg_send![&*reader, startReading];
            if !started {
                let error: Option<Retained<NSError>> = msg_send![&*reader, error];
                bail!(
                    "Cannot start Apple audio: {}",
                    error
                        .map(|e| text(&e, objc2::sel!(description)))
                        .unwrap_or_default()
                );
            }
            Ok((reader, output))
        }
    }
    fn seek(&mut self, frame: u64) -> Result<()> {
        unsafe {
            let _: () = msg_send![&*self.reader, cancelReading];
            let (reader, output) = Self::start(&self.asset, &self.track, self.format, frame)?;
            self.reader = reader;
            self.output = output;
            self.frame = frame;
            self.pending.clear();
        }
        Ok(())
    }
    fn read(&mut self, max: usize) -> Result<Option<amp_audio::PcmChunk>> {
        use amp_audio::{FramePosition, PcmChunk};
        if self.cancel.load(Ordering::Acquire) {
            bail!("Apple audio read cancelled");
        }
        if max == 0 {
            bail!("A PCM read requires at least one frame");
        }
        let max = max.min(8192);
        let channels = usize::from(self.format.channels());
        while self.pending.len() < max * channels {
            if self.cancel.load(Ordering::Acquire) {
                bail!("Apple audio read cancelled");
            }
            unsafe {
                let sample: *const SampleBuffer = msg_send![&*self.output, copyNextSampleBuffer];
                if sample.is_null() {
                    let status: isize = msg_send![&*self.reader, status];
                    if status != 2 {
                        let error: Option<Retained<NSError>> = msg_send![&*self.reader, error];
                        bail!("Apple audio decode failed: {error:?}");
                    }
                    break;
                }
                let sample = Sample(sample.cast());
                let count = CMSampleBufferGetNumSamples(sample.0);
                let block = CMSampleBufferGetDataBuffer(sample.0);
                if block.is_null() || count < 0 {
                    bail!("Invalid Apple audio buffer");
                }
                let length = CMBlockBufferGetDataLength(block);
                if length > 1024 * 1024 || length != count as usize * channels * 4 {
                    bail!("Unexpected Apple PCM buffer layout");
                }
                let mut values = vec![0f32; length / 4];
                if CMBlockBufferCopyDataBytes(block, 0, length, values.as_mut_ptr().cast()) != 0 {
                    bail!("Cannot copy Apple audio buffer");
                }
                let timestamp = CMSampleBufferGetPresentationTimeStamp(sample.0);
                let start = if timestamp.timescale > 0 {
                    ((timestamp.value as i128 * i128::from(self.format.sample_rate()))
                        / i128::from(timestamp.timescale))
                    .max(0) as u64
                } else {
                    self.frame
                };
                let expected = self.frame + (self.pending.len() / channels) as u64;
                let skip = expected.saturating_sub(start).min(count as u64) as usize * channels;
                self.pending.extend(values.into_iter().skip(skip));
            }
        }
        let count = (max * channels).min(self.pending.len());
        if count == 0 {
            return Ok(None);
        }
        let values = self.pending.drain(..count).collect();
        let chunk = PcmChunk::new(self.format, FramePosition(self.frame), values)?;
        self.frame += chunk.frame_count();
        Ok(Some(chunk))
    }
}
impl Drop for NativeReader {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.reader, cancelReading];
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    fn fixture(path: &Path) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..10000i32 {
            let value = ((frame % 1000) - 500) as i16 * 40;
            writer.write_sample(value).unwrap();
            writer.write_sample(-value).unwrap();
        }
        writer.finalize().unwrap();
    }
    #[test]
    fn native_pcm_reads_seek_and_eof() {
        objc2::rc::autoreleasepool(|_| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("input.wav");
            fixture(&path);
            let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
            let mut reader =
                NativeReader::from_url(&url, Arc::new(AtomicBool::new(false))).unwrap();
            assert_eq!(reader.format.sample_rate(), 44100);
            assert_eq!(reader.format.channels(), 2);
            let first = reader.read(37).unwrap().unwrap();
            assert_eq!(first.frame_count(), 37);
            assert!((first.samples[0] - (-20000f32 / 32768.0)).abs() < 0.0001);
            reader.seek(2345).unwrap();
            let sought = reader.read(29).unwrap().unwrap();
            assert_eq!(sought.start_frame.0, 2345);
            assert!((sought.samples[0] - (-6200f32 / 32768.0)).abs() < 0.0001);
            let mut total = 29;
            while let Some(chunk) = reader.read(127).unwrap() {
                assert!(chunk.frame_count() <= 127);
                total += chunk.frame_count();
            }
            assert_eq!(total, 10000 - 2345);
            reader.seek(0).unwrap();
            assert!(reader.read(1).unwrap().is_some());
            reader.cancel.store(true, Ordering::Release);
            assert!(reader.read(1).is_err());
        });
    }
    #[test]
    fn native_aac_and_alac_decode_seek_and_finish() {
        objc2::rc::autoreleasepool(|_| {
            let dir = tempfile::tempdir().unwrap();
            let wav = dir.path().join("input.wav");
            fixture(&wav);
            for format in ["aac", "alac"] {
                let path = dir.path().join(format!("{format}.m4a"));
                let status = std::process::Command::new("/usr/bin/afconvert")
                    .args([
                        "-f",
                        "m4af",
                        "-d",
                        if format == "aac" { "aac " } else { format },
                    ])
                    .arg(&wav)
                    .arg(&path)
                    .status()
                    .unwrap();
                assert!(status.success());
                let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
                let mut reader =
                    NativeReader::from_url(&url, Arc::new(AtomicBool::new(false))).unwrap();
                let mut frames = 0;
                while let Some(chunk) = reader.read(233).unwrap() {
                    frames += chunk.frame_count();
                }
                assert_eq!(frames, 10000, "{format} must trim priming and padding");
                reader.seek(2345).unwrap();
                let chunk = reader.read(29).unwrap().unwrap();
                assert_eq!(chunk.start_frame.0, 2345);
                let tolerance = if format == "alac" { 0.0001 } else { 0.15 };
                assert!(
                    (chunk.samples[0] - (-6200f32 / 32768.0)).abs() < tolerance,
                    "{format}: {}",
                    chunk.samples[0]
                );
            }
        });
    }
    #[test]
    fn native_read_and_seek_bounds_fail_cleanly() {
        objc2::rc::autoreleasepool(|_| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("bounds.wav");
            fixture(&path);
            let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
            let mut reader =
                NativeReader::from_url(&url, Arc::new(AtomicBool::new(false))).unwrap();
            assert!(reader.read(0).is_err());
            assert_eq!(
                reader.read(usize::MAX).unwrap().unwrap().frame_count(),
                8192
            );
            assert!(reader.seek(u64::MAX).is_err());
            reader.seek(0).unwrap();
            assert_eq!(reader.read(1).unwrap().unwrap().start_frame.0, 0);
        });
    }
}
