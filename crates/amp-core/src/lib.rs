//! UI-free application values. Provider identifiers are opaque outside their adapter.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemId {
    pub library: String,
    pub provider: String,
    pub item: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaAccess {
    File { locator: String },
    DiscTrack { cue: String, number: u8 },
    Managed { provider: String, item: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: ItemId,
    pub title: String,
    pub artist: String,
    pub number: u32,
    pub disc: u32,
    pub duration_seconds: f64,
    pub access: MediaAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: ItemId,
    pub title: String,
    pub artist: String,
    pub tracks: Vec<Track>,
    pub source_label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Library {
    pub id: String,
    pub root: String,
    pub albums: Vec<Album>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DumpPreference {
    #[default]
    Redumper,
    BinCue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    pub dump_preference: DumpPreference,
    pub automatic_metadata: bool,
    pub reduced_motion: bool,
    pub text_scale: f32,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            dump_preference: DumpPreference::Redumper,
            automatic_metadata: true,
            reduced_motion: false,
            text_scale: 1.0,
        }
    }
}

/// A provider can enumerate native or remote libraries without exposing file paths.
/// Implementations do blocking work off the presentation and audio threads.
pub trait LibraryProvider {
    type Error;
    fn scan(&self, cancelled: &dyn Fn() -> bool) -> Result<Library, Self::Error>;
}

/// These are independent: platform-managed playback need not expose PCM or seeking.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackCapabilities {
    pub seek: bool,
    pub pcm: bool,
    pub gapless: bool,
}

/// Transient queue. No saved playlists or source-file mutations.
#[derive(Debug, Default)]
pub struct UpNext(Vec<Track>);
impl UpNext {
    pub fn push(&mut self, track: Track) {
        self.0.push(track);
    }
    pub fn take(&mut self) -> Vec<Track> {
        std::mem::take(&mut self.0)
    }
    pub fn tracks(&self) -> &[Track] {
        &self.0
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
}
