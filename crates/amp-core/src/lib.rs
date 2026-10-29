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
    /// Refresh uncertainty never determines whether playback may be attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_status: Option<ScanStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<PlaybackBlockReason>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanStatus {
    Unverified { path: String, reason: String },
    MetadataUnreadable { reason: String },
}
impl ScanStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unverified { .. } => "Not verified",
            Self::MetadataUnreadable { .. } => "Couldn’t read metadata",
        }
    }
    pub fn reason(&self) -> &str {
        match self {
            Self::Unverified { reason, .. } | Self::MetadataUnreadable { reason } => reason,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanIssue {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanSummary {
    pub source_identity: Option<String>,
    /// Mounted source boundaries within the selected folder, including its root.
    #[serde(default)]
    pub mounts: std::collections::BTreeMap<String, String>,
    pub incomplete: bool,
    pub source_disconnected: bool,
    pub added: usize,
    pub removed: usize,
    pub unverified: usize,
    pub issues: Vec<ScanIssue>,
}
impl ScanSummary {
    pub fn message(&self) -> String {
        if self.source_disconnected {
            format!(
                "Refresh interrupted — source unavailable; no tracks removed. {} tracks added.",
                self.added
            )
        } else if self.incomplete {
            format!(
                "Refresh incomplete · {} tracks added · {} tracks removed. Some folders or files couldn’t be checked.",
                self.added, self.removed
            )
        } else {
            format!(
                "Refresh complete · {} tracks added · {} tracks removed",
                self.added, self.removed
            )
        }
    }
}

/// A provider explains why an otherwise browsable item cannot be played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackBlockReason {
    Protected,
    NotLocal,
    AccessDenied,
    Missing,
    UnsupportedFormat,
}
impl PlaybackBlockReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::Protected => "Protected music unsupported",
            Self::NotLocal => "Not on this device",
            Self::AccessDenied => "Music library access denied",
            Self::Missing => "Music item no longer available",
            Self::UnsupportedFormat => "Unsupported audio format",
        }
    }
}
impl Track {
    pub fn playback_block(&self) -> Option<&'static str> {
        self.blocked_reason
            .map(PlaybackBlockReason::message)
            .or(match self.availability {
                Availability::Available | Availability::Downloaded => None,
                Availability::Preparing => Some("Track is preparing"),
                Availability::Unavailable => Some("Track unavailable"),
                Availability::Unsupported => Some("Unsupported audio format"),
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    /// Internal name provenance. Older snapshots leave this unspecified until refreshed.
    #[serde(default)]
    pub metadata: AlbumMetadata,
    pub id: ItemId,
    pub title: String,
    pub artist: String,
    pub tracks: Vec<Track>,
    pub source_label: String,
}

/// A generated name remembers its original value so a correction to a cached
/// name is never mistaken for a replaceable fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NameOrigin {
    #[default]
    Unspecified,
    Source,
    Generated(String),
    MusicBrainz,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NameOrigins {
    pub title: NameOrigin,
    pub artist: NameOrigin,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlbumMetadata {
    pub names: NameOrigins,
    /// Track item IDs are scoped by their containing album.
    pub tracks: std::collections::BTreeMap<String, NameOrigins>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan: Option<ScanSummary>,
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

/// Output effects, independent of the source and read-ahead buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioProcessingSettings {
    pub crossfeed: CrossfeedSettings,
}
impl AudioProcessingSettings {
    pub fn normalized(self) -> Self {
        Self {
            crossfeed: self.crossfeed.normalized(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossfeedSettings {
    pub enabled: bool,
    pub cutoff_hz: u32,
    /// Attenuation of the crossfeed in tenths of a dB; larger is subtler.
    pub feed_db_tenths: u32,
}
impl Default for CrossfeedSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            cutoff_hz: 700,
            feed_db_tenths: 45,
        }
    }
}
impl CrossfeedSettings {
    // These bounds match bs2b's Level validation.
    pub fn normalized(self) -> Self {
        Self {
            enabled: self.enabled,
            cutoff_hz: self.cutoff_hz.clamp(300, 2000),
            feed_db_tenths: self.feed_db_tenths.clamp(10, 150),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default, alias = "sort_by_artist")]
    pub browse_artists: bool,
    pub dump_preference: DumpPreference,
    pub automatic_metadata: bool,
    #[serde(default)]
    pub acoustid_api_key: String,
    pub reduced_motion: bool,
    pub text_scale: f32,
    #[serde(default)]
    pub color_mode: ColorMode,
    #[serde(default)]
    pub oled_mode: bool,
    #[serde(default = "default_audio_buffer_seconds")]
    pub audio_buffer_seconds: u32,
    #[serde(default)]
    pub audio_processing: AudioProcessingSettings,
}
pub const fn default_audio_buffer_seconds() -> u32 {
    15
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            browse_artists: false,
            dump_preference: DumpPreference::Redumper,
            automatic_metadata: true,
            acoustid_api_key: String::new(),
            reduced_motion: false,
            text_scale: 1.0,
            color_mode: ColorMode::Dark,
            oled_mode: false,
            audio_buffer_seconds: default_audio_buffer_seconds(),
            audio_processing: AudioProcessingSettings::default(),
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
    fn legacy_and_partial_processing_preferences_default_and_custom_values_round_trip() {
        let mut old = serde_json::to_value(Preferences::default()).unwrap();
        old.as_object_mut().unwrap().remove("audio_processing");
        let mut preferences: Preferences = serde_json::from_value(old).unwrap();
        assert_eq!(
            preferences.audio_processing,
            AudioProcessingSettings::default()
        );
        let partial: AudioProcessingSettings = serde_json::from_value(serde_json::json!({
            "crossfeed": { "enabled": true }
        }))
        .unwrap();
        assert_eq!(
            partial.crossfeed,
            CrossfeedSettings {
                enabled: true,
                ..Default::default()
            }
        );
        preferences.audio_processing.crossfeed = CrossfeedSettings {
            enabled: true,
            cutoff_hz: 1234,
            feed_db_tenths: 78,
        };
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&preferences).unwrap()).unwrap();
        assert_eq!(restored.audio_processing, preferences.audio_processing);
    }

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
    fn legacy_preferences_get_larger_buffer_and_custom_values_round_trip() {
        let mut old = serde_json::to_value(Preferences::default()).unwrap();
        old.as_object_mut().unwrap().remove("audio_buffer_seconds");
        let mut prefs: Preferences = serde_json::from_value(old).unwrap();
        assert_eq!(prefs.audio_buffer_seconds, 15);
        prefs.audio_buffer_seconds = 60;
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored.audio_buffer_seconds, 60);
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

#[cfg(test)]
mod track_compatibility_tests {
    use super::*;
    #[test]
    fn older_tracks_default_to_no_blocked_reason() {
        let mut value = serde_json::json!({
            "availability": "Available", "id": {"library":"old", "provider":"file", "item":"song"},
            "title":"Song", "artist":"Artist", "number":1, "disc":1, "duration_seconds":1.0,
            "access":{"File":{"locator":"/old/song.wav"}}
        });
        let track: Track = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(track.playback_block(), None);
        value["blocked_reason"] = serde_json::json!("Protected");
        let track: Track = serde_json::from_value(value).unwrap();
        assert_eq!(track.playback_block(), Some("Protected music unsupported"));
    }
}
