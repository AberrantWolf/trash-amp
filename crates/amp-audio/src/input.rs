//! Provider dispatch and worker-side decoded input. No native types cross this boundary.
use crate::{ByteSource, MediaResolver};
use amp_core::{MediaAccess, Track};
use anyhow::{Context, Result, bail};
pub use junk_libs_audio::{FramePosition, PcmChunk, PcmFormat};
use std::{collections::BTreeMap, sync::Arc};

pub trait PcmReader: Send {
    fn format(&self) -> PcmFormat;
    fn total_frames(&self) -> Option<u64>;
    fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>>;
    fn seek_exact(&mut self, frame: FramePosition) -> Result<()>;
}
pub enum MediaInput {
    Encoded(ByteSource),
    Decoded(Box<dyn PcmReader>),
}
/// Built by the host before playback starts; browsing never replaces it.
#[derive(Default)]
pub struct ResolverRegistry(BTreeMap<String, Arc<dyn MediaResolver>>);
impl ResolverRegistry {
    pub fn register(
        &mut self,
        id: impl Into<String>,
        resolver: Arc<dyn MediaResolver>,
    ) -> Result<()> {
        let id = id.into();
        if self.0.contains_key(&id) {
            bail!("Duplicate media provider: {id}");
        }
        self.0.insert(id, resolver);
        Ok(())
    }
    fn provider(&self, track: &Track) -> Result<&dyn MediaResolver> {
        let MediaAccess::Managed { provider, .. } = &track.access else {
            bail!("Managed media reference required");
        };
        Ok(self
            .0
            .get(provider)
            .with_context(|| format!("{provider}: playback adapter unavailable"))?
            .as_ref())
    }
}
impl MediaResolver for ResolverRegistry {
    fn resolve(&self, track: &Track) -> Result<ByteSource> {
        self.provider(track)?.resolve(track)
    }
    fn open(&self, track: &Track) -> Result<MediaInput> {
        self.provider(track)?.open(track)
    }
    fn cancel(&self) {
        for resolver in self.0.values() {
            resolver.cancel();
        }
    }
}

macro_rules! decoder {
    ($ty:ty) => {
        impl PcmReader for $ty {
            fn format(&self) -> PcmFormat {
                self.format()
            }
            fn total_frames(&self) -> Option<u64> {
                self.total_frames()
            }
            fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>> {
                Ok(self.read_frames(max)?)
            }
            fn seek_exact(&mut self, frame: FramePosition) -> Result<()> {
                Ok(self.seek_exact(frame)?)
            }
        }
    };
}
decoder!(junk_libs_audio::WavDecoder);
decoder!(junk_libs_audio::AudioDecoder);
impl PcmReader for junk_libs_audio::CuePcmDecoder {
    fn format(&self) -> PcmFormat {
        self.format()
    }
    fn total_frames(&self) -> Option<u64> {
        Some(self.total_frames())
    }
    fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>> {
        Ok(self.read_frames(max.min(588))?)
    }
    fn seek_exact(&mut self, frame: FramePosition) -> Result<()> {
        Ok(self.seek_exact(frame)?)
    }
}
impl PcmReader for junk_libs_audio::RedumperPcmDecoder {
    fn format(&self) -> PcmFormat {
        self.format()
    }
    fn total_frames(&self) -> Option<u64> {
        Some(self.total_frames())
    }
    fn read_frames(&mut self, max: usize) -> Result<Option<PcmChunk>> {
        Ok(self.read_frames(max.min(588))?.map(|chunk| chunk.pcm))
    }
    fn seek_exact(&mut self, frame: FramePosition) -> Result<()> {
        Ok(self.seek_exact(frame)?)
    }
}
