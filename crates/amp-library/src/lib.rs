//! Filesystem is one library provider; SQLite is always stored on the local device.
pub mod coverage;
pub mod matching;
pub mod metadata;
mod reconcile;
pub use reconcile::ScanResult;

use amp_core::*;
use anyhow::{Context, Result, bail};
use junk_libs_disc::{cue, cue_audio::CueAudioDisc, layout::TrackKind, redumper};
use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::{Accessor, ItemKey},
    probe::Probe,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

pub struct Store(Connection);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFolder {
    pub id: String,
    pub root: String,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS metadata_http(key TEXT PRIMARY KEY, value TEXT NOT NULL, expires INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS metadata_matches(album_id TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS folder_libraries(id TEXT PRIMARY KEY, root TEXT NOT NULL, value TEXT NOT NULL);")?;
        let store = Self(db);
        if let Some(library) = store.get::<Library>("library")? {
            store.save_library(&library)?;
        }
        Ok(store)
    }
    pub fn active_source(&self) -> Result<Option<String>> {
        self.get("active_source")
    }
    pub fn save_active_source(&self, id: &str) -> Result<()> {
        self.put("active_source", &id)
    }
    pub fn restore_active_source(&self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            self.save_active_source(id)
        } else {
            self.0
                .execute("DELETE FROM state WHERE key='active_source'", [])?;
            Ok(())
        }
    }
    pub fn provider_snapshot(
        &self,
        id: &str,
    ) -> Result<Option<(Library, BTreeMap<String, String>)>> {
        self.get(&format!("provider:{id}"))
    }
    pub fn save_provider_snapshot(
        &self,
        id: &str,
        library: &Library,
        artwork: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.put(&format!("provider:{id}"), &(library, artwork))
    }
    pub fn remove_provider_snapshot(&self, id: &str) -> Result<()> {
        self.0
            .execute("DELETE FROM state WHERE key=?", [format!("provider:{id}")])?;
        Ok(())
    }
    pub fn preferences(&self) -> Result<Preferences> {
        let mut preferences: Preferences = self.get("preferences")?.unwrap_or_default();
        preferences.audio_processing = preferences.audio_processing.normalized();
        Ok(preferences)
    }
    pub fn save_preferences(&self, p: &Preferences) -> Result<()> {
        let mut preferences = p.clone();
        preferences.audio_processing = preferences.audio_processing.normalized();
        self.put("preferences", &preferences)
    }
    pub fn library(&self) -> Result<Option<Library>> {
        match self.get::<String>("active_folder")? {
            Some(id) => self.library_by_id(&id),
            None => Ok(None),
        }
    }
    pub fn library_by_id(&self, id: &str) -> Result<Option<Library>> {
        let json: Option<String> = self
            .0
            .query_row("SELECT value FROM folder_libraries WHERE id=?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        json.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn saved_folders(&self) -> Result<Vec<SavedFolder>> {
        let mut query = self
            .0
            .prepare("SELECT id, root FROM folder_libraries ORDER BY root, id")?;
        Ok(query
            .query_map([], |r| {
                Ok(SavedFolder {
                    id: r.get(0)?,
                    root: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn select_folder(&self, id: &str) -> Result<Library> {
        let library = self
            .library_by_id(id)?
            .context("Saved folder library is unavailable")?;
        self.put("active_folder", &id)?;
        Ok(library)
    }
    /// Restore the prior selection after a canceled catalog operation.
    pub fn restore_folder_selection(&self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            self.select_folder(id)?;
        } else {
            self.0
                .execute("DELETE FROM state WHERE key='active_folder'", [])?;
        }
        Ok(())
    }
    pub fn save_library(&self, library: &Library) -> Result<()> {
        self.write_library(library, true)
    }
    /// Refresh a cached catalog without changing the selected folder.
    pub fn save_library_snapshot(&self, library: &Library) -> Result<()> {
        self.write_library(library, false)
    }
    fn write_library(&self, library: &Library, select: bool) -> Result<()> {
        if library.root.is_empty() || library.id == "demo" {
            return Ok(());
        }
        let tx = self.0.unchecked_transaction()?;
        tx.execute("INSERT INTO folder_libraries(id,root,value) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET root=excluded.root,value=excluded.value",
            params![library.id, library.root, serde_json::to_string(library)?])?;
        if select {
            tx.execute("INSERT INTO state(key,value) VALUES ('active_folder',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [serde_json::to_string(&library.id)?])?;
            tx.execute("DELETE FROM state WHERE key='library'", [])?;
        }
        tx.commit()?;
        Ok(())
    }
    fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let json: Option<String> = self
            .0
            .query_row("SELECT value FROM state WHERE key=?", [key], |r| r.get(0))
            .optional()?;
        json.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    fn put<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.0.execute("INSERT INTO state(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, serde_json::to_string(value)?])?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ScanProgress {
    pub discovered: usize,
    pub completed: usize,
    pub total: Option<usize>,
}

pub struct FilesystemProvider {
    pub root: PathBuf,
    pub preferences: Preferences,
}
impl LibraryProvider for FilesystemProvider {
    type Error = anyhow::Error;
    fn scan(&self, cancelled: &dyn Fn() -> bool) -> Result<Library> {
        self.scan_with_progress(cancelled, &mut |_| {})
    }
}
impl FilesystemProvider {
    pub fn scan_with_progress(
        &self,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(ScanProgress),
    ) -> Result<Library> {
        self.scan_with_coverage(cancelled, progress)
            .map(|scan| scan.reconcile(None))
    }
    pub fn scan_with_coverage(
        &self,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(ScanProgress),
    ) -> Result<ScanResult> {
        let root = self
            .root
            .canonicalize()
            .context("Library is unavailable; check the mount")?;
        if !root.is_dir() {
            bail!("Choose a folder")
        }
        let root_string = root
            .to_str()
            .context("Library path must be valid UTF-8")?
            .to_owned();
        let mut library = Library {
            scan: None,
            id: root_string.clone(),
            root: root_string,
            ..Library::default()
        };
        let mut coverage = coverage::enumerate(&root, cancelled, progress)?;
        let mut paths: Vec<_> = coverage.files.iter().cloned().collect();
        paths.sort();
        let total = paths.len();
        let mut completed = 0;
        progress(ScanProgress {
            discovered: total,
            completed,
            total: Some(total),
        });
        let mut claimed = HashSet::new();
        let cue_packages: HashSet<_> = paths
            .iter()
            .filter(|p| extension(p) == "cue")
            .map(|p| p.with_extension("scram"))
            .collect();
        for path in paths.iter().filter(|p| {
            extension(p) == "cue"
                || (extension(p) == "scram" && !cue_packages.contains(&p.with_extension("scram")))
        }) {
            if cancelled() {
                bail!("Scan cancelled")
            }
            completed += 1;
            match disc_album(path, &library.id, &self.preferences) {
                Ok((album, files)) => {
                    claimed.extend(files);
                    library.albums.push(album);
                }
                Err(e) => {
                    coverage.record_probe_failure(path, &e);
                    library.warnings.push(format!("{}: {e:#}", path.display()));
                }
            }
            progress(ScanProgress {
                discovered: total,
                completed,
                total: Some(total),
            });
        }
        let mut groups: BTreeMap<(PathBuf, String, Option<String>), AlbumFragment> =
            BTreeMap::new();
        let audio_paths: Vec<_> = paths
            .iter()
            .filter(|path| !claimed.contains(*path) && is_supported_audio_file(path))
            .collect();
        completed = total - audio_paths.len();
        // Four readers hide network latency without flooding a NAS with open files.
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        };
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let results = std::thread::scope(|scope| -> Result<Vec<_>> {
            let (tx, rx) = mpsc::sync_channel(16);
            for _ in 0..4.min(audio_paths.len()) {
                let (next, stop, paths, tx, id) =
                    (&next, &stop, &audio_paths, tx.clone(), &library.id);
                scope.spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        let result = tagged_track(path, id);
                        if result.as_ref().err().is_some_and(coverage::source_failure) {
                            // Stop scheduling reads after a source-level failure;
                            // already completed work can still be reconciled.
                            stop.store(true, Ordering::Release);
                        }
                        if tx.send((i, result)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut results = Vec::new();
            loop {
                if cancelled() {
                    stop.store(true, Ordering::Relaxed);
                }
                match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(result) => {
                        results.push(result);
                        completed += 1;
                        progress(ScanProgress {
                            discovered: total,
                            completed,
                            total: Some(total),
                        });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            if cancelled() {
                bail!("Scan cancelled")
            }
            results.sort_by_key(|r| r.0);
            Ok(results)
        })?;
        for (i, result) in results {
            let path = audio_paths[i];
            let result: Result<_> = match result {
                Ok(track) => Ok(track),
                Err(error) => {
                    coverage.record_probe_failure(path, &error);
                    library
                        .warnings
                        .push(format!("{}: {error:#}", path.display()));
                    match fallback_file_track(path, &library.id, &error.to_string()) {
                        Some(track) => Ok(track),
                        None => continue,
                    }
                }
            };
            match result {
                Ok((title, album_artist, tagged, track, names)) => {
                    let parent = path.parent().unwrap().to_owned();
                    let key = (
                        parent.clone(),
                        normalized(&title),
                        album_artist.as_deref().map(normalized),
                    );
                    let fragment = groups.entry(key).or_insert_with(|| AlbumFragment {
                        tagged,
                        album_artist: album_artist.clone(),
                        album: Album {
                            metadata: Default::default(),
                            id: id(
                                &library.id,
                                format!(
                                    "{}\n{title}\n{}",
                                    parent.display(),
                                    album_artist.as_deref().unwrap_or("")
                                ),
                            ),
                            title,
                            artist: String::new(),
                            tracks: vec![],
                            source_label: parent.display().to_string(),
                        },
                    });
                    if tagged {
                        fragment.album.metadata.names.title = NameOrigin::Source;
                    } else if fragment.album.metadata.names.title != NameOrigin::Source {
                        fragment.album.metadata.names.title =
                            NameOrigin::Generated(fragment.album.title.clone());
                    }
                    fragment
                        .album
                        .metadata
                        .tracks
                        .insert(track.id.item.clone(), names);
                    fragment.album.tracks.push(track);
                }
                Err(e) => library.warnings.push(format!("{}: {e:#}", path.display())),
            }
        }
        for mut fragment in merge_album_fragments(groups.into_values().collect()) {
            let album = &mut fragment.album;
            let explicit_artist = fragment.album_artist.is_some();
            album.artist = fragment.album_artist.unwrap_or_else(|| {
                let first = album
                    .tracks
                    .first()
                    .map(|t| t.artist.as_str())
                    .unwrap_or("Unknown artist");
                if album
                    .tracks
                    .iter()
                    .all(|t| normalized(&t.artist) == normalized(first))
                {
                    first.to_owned()
                } else {
                    "Various artists".into()
                }
            });
            // A common track credit is a source name; the compilation label and
            // unknown-artist text are generated display fallbacks.
            album.metadata.names.artist = if !explicit_artist
                && (album.artist == "Various artists" || album.artist == "Unknown artist")
            {
                NameOrigin::Generated(album.artist.clone())
            } else {
                NameOrigin::Source
            };
            album.tracks.sort_by(|a, b| {
                (a.disc, a.number, &a.id.item).cmp(&(b.disc, b.number, &b.id.item))
            });
            library.albums.push(fragment.album);
        }
        library.albums.sort_by(|a, b| {
            (&a.artist, &a.title, &a.id.item).cmp(&(&b.artist, &b.title, &b.id.item))
        });
        progress(ScanProgress {
            discovered: total,
            completed,
            total: Some(total),
        });
        coverage.check_source();
        if cancelled() {
            bail!("Scan cancelled");
        }
        Ok(ScanResult {
            library,
            coverage: Some(coverage),
        })
    }
}
/// Folder/artist boundaries are evidence, not album identity. Merge complementary
/// tagged fragments, but keep conflicting copies and explicit album artists apart.
struct AlbumFragment {
    album: Album,
    album_artist: Option<String>,
    tagged: bool,
}
fn normalized(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn merge_album_fragments(fragments: Vec<AlbumFragment>) -> Vec<AlbumFragment> {
    let mut groups: BTreeMap<String, Vec<AlbumFragment>> = BTreeMap::new();
    for fragment in fragments {
        let candidates = groups.entry(normalized(&fragment.album.title)).or_default();
        let compatible = |existing: &&mut AlbumFragment| {
            existing.tagged
                && fragment.tagged
                && match (&existing.album_artist, &fragment.album_artist) {
                    (Some(a), Some(b)) => normalized(a) == normalized(b),
                    _ => true,
                }
                && !existing.album.tracks.iter().any(|a| {
                    fragment.album.tracks.iter().any(|b| {
                        if a.number > 0 && b.number > 0 {
                            (a.disc, a.number) == (b.disc, b.number)
                        } else {
                            normalized(&a.title) == normalized(&b.title)
                        }
                    })
                })
        };
        if let Some(existing) = candidates.iter_mut().find(compatible) {
            existing
                .album
                .metadata
                .tracks
                .extend(fragment.album.metadata.tracks);
            existing.album.tracks.extend(fragment.album.tracks);
            if existing.album_artist.is_none() {
                existing.album_artist = fragment.album_artist;
            }
            existing.album.source_label = "Merged from multiple folders".into();
        } else {
            candidates.push(fragment);
        }
    }
    groups.into_values().flatten().collect()
}

/// Standalone formats shared by library scanning and transient file playback.
pub fn is_supported_audio_file(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "flac" | "mp3" | "wav" | "aiff" | "aif" | "ogg" | "m4a" | "aac" | "opus"
    )
}

/// Read local tags without opening a store or enriching/saving library metadata.
pub fn local_file_track(path: &Path, session: &str) -> Result<Track> {
    tagged_track(path, session).map(|(_, _, _, track, _)| track)
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}
fn id(library: &str, item: String) -> ItemId {
    ItemId {
        library: library.into(),
        provider: "filesystem".into(),
        item,
    }
}
type TaggedTrack = (String, Option<String>, bool, Track, NameOrigins);

// A failed tag read should not hide a recognizable new audio file. Unknown
// headers and inaccessible files are reported without inventing playable items.
fn fallback_file_track(path: &Path, library: &str, reason: &str) -> Option<TaggedTrack> {
    let probe = Probe::new(std::io::BufReader::new(std::fs::File::open(path).ok()?))
        .guess_file_type()
        .ok()?;
    probe.file_type()?;
    let locator = path.to_str()?.to_owned();
    let title = path.file_stem()?.to_string_lossy().into_owned();
    let album = path.parent()?.file_name()?.to_string_lossy().into_owned();
    let names = NameOrigins {
        title: NameOrigin::Generated(title.clone()),
        artist: NameOrigin::Generated("Unknown artist".into()),
    };
    Some((
        album,
        None,
        false,
        Track {
            scan_status: Some(ScanStatus::MetadataUnreadable {
                reason: reason.into(),
            }),
            blocked_reason: None,
            availability: Default::default(),
            source: None,
            id: id(library, locator.clone()),
            title,
            artist: "Unknown artist".into(),
            number: 0,
            disc: 1,
            duration_seconds: 0.0,
            access: MediaAccess::File { locator },
        },
        names,
    ))
}
fn tagged_track(path: &Path, library: &str) -> Result<TaggedTrack> {
    let file = Probe::open(path)?
        .options(lofty::config::ParseOptions::new().read_cover_art(false))
        .read()?;
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let folder = path
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let title = tag
        .and_then(|t| t.album())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.into_owned())
        .unwrap_or(folder);
    let tagged = tag
        .and_then(|t| t.album())
        .is_some_and(|s| !s.trim().is_empty());
    let album_artist = tag
        .and_then(|t| t.get_string(ItemKey::AlbumArtist))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);
    let locator = path
        .to_str()
        .context("Track path must be valid UTF-8")?
        .to_owned();
    let track = Track {
        scan_status: None,
        blocked_reason: None,
        availability: Default::default(),
        source: None,
        id: id(library, locator.clone()),
        title: tag
            .and_then(|t| t.title())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
        artist: tag
            .and_then(|t| t.artist())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| {
                album_artist
                    .clone()
                    .unwrap_or_else(|| "Unknown artist".into())
            }),
        number: tag.and_then(|t| t.track()).unwrap_or(0),
        disc: tag.and_then(|t| t.disk()).unwrap_or(1),
        duration_seconds: file.properties().duration().as_secs_f64(),
        access: MediaAccess::File { locator },
    };
    let names = NameOrigins {
        title: if tag
            .and_then(|t| t.title())
            .is_some_and(|s| !s.trim().is_empty())
        {
            NameOrigin::Source
        } else {
            NameOrigin::Generated(track.title.clone())
        },
        artist: if tag
            .and_then(|t| t.artist())
            .is_some_and(|s| !s.trim().is_empty())
            || album_artist.is_some()
        {
            NameOrigin::Source
        } else {
            NameOrigin::Generated(track.artist.clone())
        },
    };
    Ok((title, album_artist, tagged, track, names))
}
fn disc_album(
    path: &Path,
    library: &str,
    preferences: &Preferences,
) -> Result<(Album, Vec<PathBuf>)> {
    let cue_anchor = if extension(path) == "cue" {
        path.to_owned()
    } else {
        path.with_extension("cue")
    };
    let path = cue_anchor.as_path();
    let text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let parent = path.parent().unwrap();
    let files = if text.is_empty() {
        vec![]
    } else {
        cue::parse_cue(&text)?
            .files
            .iter()
            .map(|f| cue::resolve_local_file(parent, &f.filename))
            .collect::<Result<Vec<_>, _>>()?
    };
    let sidecars = redumper::find_sidecars(path);
    let raw_present = sidecars.scram.is_some();
    let bin_layout = || -> Result<Vec<(u8, f64, bool)>> {
        Ok(CueAudioDisc::open(path)?
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .map(|t| {
                (
                    t.number,
                    (t.end_frame - t.start_frame) as f64 / 44100.0,
                    t.pre_emphasis,
                )
            })
            .collect())
    };
    let raw_layout = || -> Result<Vec<(u8, f64, bool)>> {
        Ok(redumper::pcm::read_audio_layout(&sidecars)?
            .iter()
            .map(|t| {
                (
                    t.number,
                    (t.end_frame - t.start_frame) as f64 / 44100.0,
                    t.pre_emphasis,
                )
            })
            .collect())
    };
    let (layouts, use_raw) =
        if raw_present && preferences.dump_preference == DumpPreference::Redumper {
            (raw_layout()?, true)
        } else {
            match bin_layout() {
                Ok(l) => (l, false),
                Err(e) if raw_present => (
                    raw_layout().with_context(|| format!("CUE layout: {e:#}"))?,
                    true,
                ),
                Err(e) => return Err(e),
            }
        };
    let has_emphasis = layouts.iter().any(|t| t.2);
    let cdtext = sidecars
        .cdtext
        .as_ref()
        .and_then(|p| redumper::parse_cdtext(p).ok());
    let block = cdtext.as_ref().and_then(|c| c.primary());
    // CUE's structural parser intentionally ignores text fields. Preserve album-level fields here.
    let header = text.split("TRACK ").next().unwrap_or("");
    let field = |key: &str| {
        header.lines().find_map(|l| {
            l.trim()
                .strip_prefix(key)
                .map(|s| s.trim().trim_matches('"').to_owned())
        })
    };
    let source_title = block
        .and_then(|b| b.title.clone())
        .or_else(|| field("TITLE "))
        .filter(|s| !s.trim().is_empty());
    let title = source_title.clone().unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let source_artist = block
        .and_then(|b| b.performer.clone())
        .or_else(|| field("PERFORMER "))
        .filter(|s| !s.trim().is_empty());
    let artist = source_artist
        .clone()
        .unwrap_or_else(|| "Unknown artist".into());
    let cue_path = path
        .to_str()
        .context("CUE path must be valid UTF-8")?
        .to_owned();
    let mut cue_names: BTreeMap<u8, (Option<String>, Option<String>)> = BTreeMap::new();
    let mut current = None;
    for line in text.lines() {
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let keyword = parts.next().unwrap_or("").to_ascii_uppercase();
        let rest = parts.next().unwrap_or("").trim();
        if keyword == "TRACK" {
            current = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u8>().ok());
        } else if let Some(number) = current {
            let names = cue_names.entry(number).or_default();
            let value = rest.trim_matches('"').trim();
            if !value.is_empty() {
                match keyword.as_str() {
                    "TITLE" => names.0 = Some(value.to_owned()),
                    "PERFORMER" => names.1 = Some(value.to_owned()),
                    _ => {}
                }
            }
        }
    }
    let tracks: Vec<Track> = layouts
        .iter()
        .map(|l| Track {
            scan_status: None,
            blocked_reason: None,
            availability: Default::default(),
            source: None,
            id: id(library, format!("{cue_path}#{}", l.0)),
            title: block
                .and_then(|b| b.tracks.iter().find(|t| t.track_number == l.0))
                .and_then(|t| t.title.clone())
                .or_else(|| cue_names.get(&l.0).and_then(|n| n.0.clone()))
                .unwrap_or_else(|| format!("Track {}", l.0)),
            artist: block
                .and_then(|b| b.tracks.iter().find(|t| t.track_number == l.0))
                .and_then(|t| t.performer.clone())
                .or_else(|| cue_names.get(&l.0).and_then(|n| n.1.clone()))
                .unwrap_or_else(|| artist.clone()),
            number: l.0 as u32,
            disc: 1,
            duration_seconds: l.1,
            access: if use_raw {
                MediaAccess::RedumperTrack {
                    anchor: cue_path.clone(),
                    number: l.0,
                }
            } else {
                MediaAccess::DiscTrack {
                    cue: cue_path.clone(),
                    number: l.0,
                }
            },
        })
        .collect();
    let metadata = AlbumMetadata {
        names: NameOrigins {
            title: if source_title.is_some() {
                NameOrigin::Source
            } else {
                NameOrigin::Generated(title.clone())
            },
            artist: if source_artist.is_some() {
                NameOrigin::Source
            } else {
                NameOrigin::Generated(artist.clone())
            },
        },
        tracks: tracks
            .iter()
            .map(|track| {
                let number = track.number as u8;
                let cdtrack =
                    block.and_then(|b| b.tracks.iter().find(|t| t.track_number == number));
                let has_title = cdtrack.and_then(|t| t.title.as_ref()).is_some()
                    || cue_names.get(&number).and_then(|n| n.0.as_ref()).is_some();
                let has_artist = cdtrack.and_then(|t| t.performer.as_ref()).is_some()
                    || cue_names.get(&number).and_then(|n| n.1.as_ref()).is_some()
                    || source_artist.is_some();
                (
                    track.id.item.clone(),
                    NameOrigins {
                        title: if has_title {
                            NameOrigin::Source
                        } else {
                            NameOrigin::Generated(track.title.clone())
                        },
                        artist: if has_artist {
                            NameOrigin::Source
                        } else {
                            NameOrigin::Generated(track.artist.clone())
                        },
                    },
                )
            })
            .collect(),
    };
    let label = format!(
        "{}{}",
        if use_raw { "Redumper raw" } else { "BIN/CUE" },
        if has_emphasis {
            " • pre-emphasis present; de-emphasis not yet applied"
        } else {
            ""
        }
    );
    Ok((
        Album {
            metadata,
            id: id(library, cue_path),
            title,
            artist,
            tracks,
            source_label: label,
        },
        files,
    ))
}

pub fn demo_library() -> Library {
    Library {
        scan: None,
        id: "demo".into(),
        root: "Layout demo — generated albums, no audio".into(),
        albums: (0..2000)
            .map(|i| Album {
                metadata: Default::default(),
                id: id("demo", i.to_string()),
                title: format!(
                    "{} {:04}",
                    [
                        "Night transit",
                        "Quiet circuitry",
                        "Rooms for listening",
                        "A distant signal"
                    ][i % 4],
                    i + 1
                ),
                artist: [
                    "The Late Hours",
                    "Silver Current",
                    "Paper Satellites",
                    "Low Frequency",
                ][i % 4]
                    .into(),
                source_label: "Generated layout fixture".into(),
                tracks: (1..=10)
                    .map(|n| Track {
                        scan_status: None,
                        blocked_reason: None,
                        availability: Default::default(),
                        source: None,
                        id: id("demo", format!("{i}/{n}")),
                        title: format!("Movement {n}"),
                        artist: "Layout fixture".into(),
                        number: n,
                        disc: 1,
                        duration_seconds: 180.0,
                        access: MediaAccess::Managed {
                            provider: "demo".into(),
                            item: format!("{i}/{n}"),
                        },
                    })
                    .collect(),
            })
            .collect(),
        warnings: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_metadata_reads_keep_known_tracks_but_do_not_invent_audio_from_extensions() {
        let directory = tempfile::tempdir().unwrap();
        write_track(
            directory.path(),
            "album/song.wav",
            Some("Known album"),
            "Artist",
            1,
            None,
        );
        let provider = FilesystemProvider {
            root: directory.path().into(),
            preferences: Default::default(),
        };
        let original = provider
            .scan_with_coverage(&|| false, &mut |_| {})
            .unwrap()
            .reconcile(None);
        let path = directory.path().join("album/song.wav");
        std::fs::write(&path, b"not an audio header").unwrap();
        std::fs::write(directory.path().join("pretend.wav"), b"also not audio").unwrap();
        assert!(fallback_file_track(&path, &original.id, "bad tags").is_none());
        let refreshed = provider
            .scan_with_coverage(&|| false, &mut |_| {})
            .unwrap()
            .reconcile(Some(&original));
        assert_eq!(refreshed.albums.len(), 1);
        let track = &refreshed.albums[0].tracks[0];
        assert_eq!(track.title, "Song 1");
        assert!(matches!(
            track.scan_status,
            Some(ScanStatus::MetadataUnreadable { .. })
        ));
        assert!(track.playback_block().is_none());
        assert_eq!(refreshed.scan.as_ref().unwrap().added, 0);
        assert_eq!(refreshed.scan.as_ref().unwrap().removed, 0);
        write_track(
            directory.path(),
            "album/song.wav",
            Some("Known album"),
            "Artist",
            1,
            None,
        );
        std::fs::remove_file(directory.path().join("pretend.wav")).unwrap();
        let repaired = provider
            .scan_with_coverage(&|| false, &mut |_| {})
            .unwrap()
            .reconcile(Some(&refreshed));
        assert!(repaired.albums[0].tracks[0].scan_status.is_none());
        assert!(!repaired.scan.unwrap().incomplete);
    }
    #[test]
    fn refreshing_an_inactive_catalog_does_not_select_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("cache.sqlite3")).unwrap();
        let mut a = Library {
            scan: None,
            id: "a".into(),
            root: "/offline/a".into(),
            ..Default::default()
        };
        let b = Library {
            scan: None,
            id: "b".into(),
            root: "/offline/b".into(),
            ..Default::default()
        };
        store.save_library(&a).unwrap();
        store.save_library(&b).unwrap();
        a.warnings.push("updated snapshot".into());
        store.save_library_snapshot(&a).unwrap();
        assert_eq!(store.library().unwrap().unwrap().id, "b");
        assert_eq!(
            store.library_by_id("a").unwrap().unwrap().warnings,
            ["updated snapshot"]
        );
    }

    #[test]
    fn progress_counts_all_files_and_finishes_monotonically() {
        let dir = tempfile::tempdir().unwrap();
        for n in 1..=12 {
            write_track(
                dir.path(),
                &format!("{n}.wav"),
                Some("Album"),
                "Artist",
                n,
                Some("Album Artist"),
            );
        }
        std::fs::write(dir.path().join("notes.txt"), "notes").unwrap();
        std::fs::write(dir.path().join("bad.flac"), "invalid").unwrap();
        let mut events = Vec::new();
        let library = FilesystemProvider {
            root: dir.path().into(),
            preferences: Preferences::default(),
        }
        .scan_with_progress(&|| false, &mut |p| events.push(p))
        .unwrap();
        assert_eq!(library.albums[0].artist, "Album Artist");
        assert_eq!(library.albums[0].tracks.len(), 12);
        assert_eq!(library.warnings.len(), 1);
        let last = events.last().unwrap();
        assert_eq!(last.total, Some(14));
        assert_eq!(last.completed, 14);
        assert!(events.windows(2).all(|w| w[0].completed <= w[1].completed));
        assert!(
            events
                .iter()
                .all(|p| p.completed <= p.total.unwrap_or(usize::MAX))
        );
    }
    #[test]
    fn cancellation_during_parallel_reads_drains_workers() {
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        for n in 1..=20 {
            write_track(
                dir.path(),
                &format!("{n}.wav"),
                Some("Album"),
                "Artist",
                n,
                None,
            );
        }
        let stop = Cell::new(false);
        let result = FilesystemProvider {
            root: dir.path().into(),
            preferences: Preferences::default(),
        }
        .scan_with_progress(&|| stop.get(), &mut |p| {
            if p.completed > 0 {
                stop.set(true);
            }
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }
    #[test]
    fn crossfeed_preferences_normalize_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.sqlite3");
        let store = Store::open(&path).unwrap();
        let mut preferences = Preferences::default();
        preferences.audio_processing.crossfeed = amp_core::CrossfeedSettings {
            enabled: true,
            cutoff_hz: 1234,
            feed_db_tenths: 78,
        };
        store.save_preferences(&preferences).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.preferences().unwrap().audio_processing,
            preferences.audio_processing
        );
        preferences.audio_processing.crossfeed.cutoff_hz = 0;
        preferences.audio_processing.crossfeed.feed_db_tenths = u32::MAX;
        // Check both boundaries: even legacy/unvalidated database entries load safely.
        store.put("preferences", &preferences).unwrap();
        assert_eq!(
            store.preferences().unwrap().audio_processing,
            preferences.audio_processing.normalized()
        );
        store.save_preferences(&preferences).unwrap();
        assert_eq!(
            store
                .get::<Preferences>("preferences")
                .unwrap()
                .unwrap()
                .audio_processing,
            preferences.audio_processing.normalized()
        );
    }

    #[test]
    fn preferences_and_folder_libraries_survive_switching_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.preferences().unwrap().dump_preference,
            DumpPreference::Redumper
        );
        let p = Preferences {
            dump_preference: DumpPreference::BinCue,
            ..Preferences::default()
        };
        store.save_preferences(&p).unwrap();
        assert_eq!(
            store.preferences().unwrap().dump_preference,
            DumpPreference::BinCue
        );
        store
            .save_library(&Library {
                scan: None,
                id: "a".into(),
                root: "/missing/a".into(),
                ..Library::default()
            })
            .unwrap();
        store
            .save_library(&Library {
                scan: None,
                id: "b".into(),
                root: "/missing/b".into(),
                ..Library::default()
            })
            .unwrap();
        assert_eq!(store.library().unwrap().unwrap().id, "b");
        assert_eq!(store.saved_folders().unwrap().len(), 2);
        store.select_folder("a").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.library().unwrap().unwrap().root, "/missing/a");
        assert_eq!(
            store.library_by_id("b").unwrap().unwrap().root,
            "/missing/b"
        );
        assert!(store.select_folder("unknown").is_err());
        assert_eq!(store.library().unwrap().unwrap().id, "a");
        let mut a = store.library().unwrap().unwrap();
        a.warnings.push("refreshed".into());
        store.save_library(&a).unwrap();
        assert_eq!(store.saved_folders().unwrap().len(), 2);
        assert!(
            store
                .library_by_id("b")
                .unwrap()
                .unwrap()
                .warnings
                .is_empty()
        );
        store.save_library(&Library::default()).unwrap();
        store.save_library(&demo_library()).unwrap();
        assert_eq!(store.library().unwrap().unwrap().warnings, ["refreshed"]);
    }
    #[test]
    fn legacy_library_migrates_without_losing_preferences_or_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = Store::open(&path).unwrap();
        let legacy = Library {
            scan: None,
            id: "old".into(),
            root: "/offline/music".into(),
            ..Library::default()
        };
        store.put("library", &legacy).unwrap();
        let preferences = Preferences {
            reduced_motion: true,
            ..Preferences::default()
        };
        store.save_preferences(&preferences).unwrap();
        store
            .0
            .execute(
                "INSERT INTO metadata_http VALUES ('test','response',123)",
                [],
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.library().unwrap().unwrap().id, "old");
        assert_eq!(store.saved_folders().unwrap().len(), 1);
        assert!(store.preferences().unwrap().reduced_motion);
        assert!(store.get::<Library>("library").unwrap().is_none());
        assert_eq!(
            store
                .0
                .query_row(
                    "SELECT value FROM metadata_http WHERE key='test'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "response"
        );
        drop(store);
        assert_eq!(
            Store::open(&path).unwrap().saved_folders().unwrap().len(),
            1
        );
    }
    #[test]
    fn unavailable_root_does_not_become_empty_library() {
        let dir = tempfile::tempdir().unwrap();
        let p = FilesystemProvider {
            root: dir.path().join("missing"),
            preferences: Preferences::default(),
        };
        assert!(p.scan(&|| false).is_err());
    }
    #[test]
    fn scan_can_be_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let p = FilesystemProvider {
            root: dir.path().into(),
            preferences: Preferences::default(),
        };
        assert!(p.scan(&|| true).is_err());
    }
    fn write_track(
        root: &Path,
        relative: &str,
        album: Option<&str>,
        artist: &str,
        number: u32,
        album_artist: Option<&str>,
    ) {
        use lofty::{
            config::WriteOptions,
            prelude::TagExt,
            tag::{Tag, TagType},
        };
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut wav = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        wav.write_sample(0i16).unwrap();
        wav.write_sample(0i16).unwrap();
        wav.finalize().unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        if let Some(album) = album {
            tag.set_album(album.into());
        }
        tag.set_artist(artist.into());
        tag.set_title(format!("Song {number}"));
        tag.set_track(number);
        if let Some(artist) = album_artist {
            tag.insert_text(ItemKey::AlbumArtist, artist.into());
        }
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
    }
    fn scan_fixture(root: &Path) -> Library {
        FilesystemProvider {
            root: root.into(),
            preferences: Preferences::default(),
        }
        .scan(&|| false)
        .unwrap()
    }
    #[test]
    fn compilation_tracks_merge_across_artist_first_paths() {
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            "Artist A/Collection/one.wav",
            Some("Collection"),
            "Artist A",
            1,
            None,
        );
        write_track(
            dir.path(),
            "Artist B/Collection/two.wav",
            Some("Collection"),
            "Artist B",
            2,
            None,
        );
        let library = scan_fixture(dir.path());
        assert!(library.warnings.is_empty(), "{:?}", library.warnings);
        assert_eq!(library.albums.len(), 1);
        assert_eq!(library.albums[0].artist, "Various artists");
        assert_eq!(library.albums[0].tracks.len(), 2);
        assert_eq!(library.albums[0].tracks[0].artist, "Artist A");
    }
    #[test]
    fn compilation_in_one_folder_does_not_split_by_track_artist() {
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            "Collection/one.wav",
            Some("Collection"),
            "Artist A",
            1,
            None,
        );
        write_track(
            dir.path(),
            "Collection/two.wav",
            Some("Collection"),
            "Artist B",
            2,
            None,
        );
        assert_eq!(scan_fixture(dir.path()).albums.len(), 1);
    }
    #[test]
    fn duplicated_track_sets_and_distinct_album_artists_stay_separate() {
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            "copy-a/one.wav",
            Some("Collection"),
            "Artist",
            1,
            None,
        );
        write_track(
            dir.path(),
            "copy-b/one.wav",
            Some("Collection"),
            "Artist",
            1,
            None,
        );
        assert_eq!(scan_fixture(dir.path()).albums.len(), 2);
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            "a/one.wav",
            Some("Greatest Hits"),
            "Artist A",
            1,
            Some("Artist A"),
        );
        write_track(
            dir.path(),
            "b/two.wav",
            Some("Greatest Hits"),
            "Artist B",
            2,
            Some("Artist B"),
        );
        assert_eq!(scan_fixture(dir.path()).albums.len(), 2);
    }
    #[test]
    fn untagged_folder_names_are_not_enough_to_merge_albums() {
        let dir = tempfile::tempdir().unwrap();
        write_track(dir.path(), "a/Music/one.wav", None, "Artist A", 1, None);
        write_track(dir.path(), "b/Music/two.wav", None, "Artist B", 2, None);
        assert_eq!(scan_fixture(dir.path()).albums.len(), 2);
    }
    #[test]
    fn raw_only_discovery_preference_and_invalid_raw_do_not_silently_switch() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("disc.cue");
        let samples = (45150u64 + 1) * 588;
        std::fs::File::create(anchor.with_extension("scram"))
            .unwrap()
            .set_len(samples * 4)
            .unwrap();
        std::fs::File::create(anchor.with_extension("state"))
            .unwrap()
            .set_len(samples)
            .unwrap();
        std::fs::File::create(anchor.with_extension("subcode"))
            .unwrap()
            .set_len((45150 + 1) * 96)
            .unwrap();
        let mut toc = vec![0, 18, 1, 1];
        for (n, lba) in [(1, 0i32), (0xaa, 1)] {
            toc.extend([0, 0x10, n, 0]);
            toc.extend(lba.to_be_bytes());
        }
        std::fs::write(anchor.with_extension("toc"), toc).unwrap();
        let mut provider = FilesystemProvider {
            root: dir.path().into(),
            preferences: Preferences::default(),
        };
        let library = provider.scan(&|| false).unwrap();
        assert!(library.warnings.is_empty(), "{:?}", library.warnings);
        assert_eq!(library.albums.len(), 1);
        assert!(matches!(
            library.albums[0].tracks[0].access,
            MediaAccess::RedumperTrack { .. }
        ));
        assert_eq!(library.albums[0].tracks[0].duration_seconds, 1.0 / 75.0);
        std::fs::write(
            &anchor,
            "FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n INDEX 01 00:00:00\n",
        )
        .unwrap();
        std::fs::write(anchor.with_extension("bin"), vec![0; 2352]).unwrap();
        let library = provider.scan(&|| false).unwrap();
        assert_eq!(library.albums.len(), 1);
        provider.preferences.dump_preference = DumpPreference::BinCue;
        let library = provider.scan(&|| false).unwrap();
        assert!(matches!(
            library.albums[0].tracks[0].access,
            MediaAccess::DiscTrack { .. }
        ));
        std::fs::remove_file(anchor.with_extension("state")).unwrap();
        provider.preferences.dump_preference = DumpPreference::Redumper;
        let library = provider.scan(&|| false).unwrap();
        assert!(library.albums.is_empty());
        assert_eq!(library.warnings.len(), 1);
        assert!(library.warnings[0].contains("missing .state"));
    }
}

#[cfg(test)]
mod transient_file_tests {
    use super::*;
    #[test]
    fn local_track_uses_filename_without_creating_library_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("A song.WAV");
        let mut wav = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        wav.write_sample(0i16).unwrap();
        wav.write_sample(0i16).unwrap();
        wav.finalize().unwrap();
        assert!(is_supported_audio_file(&path));
        let track = local_file_track(&path, "drop:unique").unwrap();
        assert_eq!(track.title, "A song");
        assert_eq!(track.artist, "Unknown artist");
        assert_eq!(track.id.library, "drop:unique");
        assert!(
            matches!(track.access, MediaAccess::File { locator } if locator == path.to_str().unwrap())
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
