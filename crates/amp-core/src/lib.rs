//! UI-free application values. Provider identifiers are opaque outside their adapter.
use serde::{Deserialize, Serialize};
pub mod browse;

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
    RedumperTrack { anchor: String, number: u8 },
    Managed { provider: String, item: String },
}

/// Availability is supplied by a provider, independently of descriptive labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    #[default]
    Available,
    Downloaded,
    Preparing,
    Unavailable,
    Unsupported,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCoordinates {
    pub connection: String,
    pub collection: String,
    pub item: String,
    pub capture: String,
    pub representation: String,
    pub track: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub availability: Availability,
    #[serde(default)]
    pub source: Option<SourceCoordinates>,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    Light,
    #[default]
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default, alias = "sort_by_artist")]
    pub browse_artists: bool,
    pub dump_preference: DumpPreference,
    pub automatic_metadata: bool,
    pub reduced_motion: bool,
    pub text_scale: f32,
    #[serde(default)]
    pub color_mode: ColorMode,
    #[serde(default)]
    pub oled_mode: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            browse_artists: false,
            dump_preference: DumpPreference::Redumper,
            automatic_metadata: true,
            reduced_motion: false,
            text_scale: 1.0,
            color_mode: ColorMode::Dark,
            oled_mode: false,
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

#[cfg(test)]
mod preference_tests {
    use super::*;
    #[test]
    fn appearance_defaults_for_legacy_preferences_and_round_trips() {
        let mut old = serde_json::to_value(Preferences::default()).unwrap();
        old.as_object_mut().unwrap().remove("color_mode");
        old.as_object_mut().unwrap().remove("oled_mode");
        let mut prefs: Preferences = serde_json::from_value(old).unwrap();
        assert_eq!(prefs.color_mode, ColorMode::Dark);
        assert!(!prefs.oled_mode);
        for mode in [ColorMode::Light, ColorMode::Dark] {
            prefs.color_mode = mode;
            prefs.oled_mode = true;
            let restored: Preferences =
                serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
            assert_eq!(restored.color_mode, mode);
            assert!(restored.oled_mode);
        }
    }

    #[test]
    fn legacy_sort_preference_becomes_artist_browsing() {
        let mut old = serde_json::to_value(Preferences::default()).unwrap();
        old.as_object_mut().unwrap().remove("browse_artists");
        old["sort_by_artist"] = true.into();
        let prefs: Preferences = serde_json::from_value(old).unwrap();
        assert!(prefs.browse_artists);
        let saved = serde_json::to_value(&prefs).unwrap();
        assert_eq!(saved["browse_artists"], true);
        assert!(saved.get("sort_by_artist").is_none());
    }
}
