//! Filesystem is one library provider; SQLite is always stored on the local device.
use amp_core::*;
use anyhow::{Context, Result, bail};
use junk_libs_disc::{cue, layout::TrackKind, redumper};
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
use walkdir::WalkDir;

pub struct Store(Connection);
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        Ok(Self(db))
    }
    pub fn preferences(&self) -> Result<Preferences> {
        Ok(self.get("preferences")?.unwrap_or_default())
    }
    pub fn save_preferences(&self, p: &Preferences) -> Result<()> {
        self.put("preferences", p)
    }
    pub fn library(&self) -> Result<Option<Library>> {
        self.get("library")
    }
    pub fn save_library(&self, library: &Library) -> Result<()> {
        self.put("library", library)
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
        progress: &mut dyn FnMut(usize),
    ) -> Result<Library> {
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
            id: root_string.clone(),
            root: root_string,
            ..Library::default()
        };
        let mut paths = Vec::new();
        for entry in WalkDir::new(&root).follow_links(false) {
            if cancelled() {
                bail!("Scan cancelled")
            }
            match entry {
                Ok(e) if e.file_type().is_file() => paths.push(e.into_path()),
                Err(e) => library.warnings.push(e.to_string()),
                _ => {}
            }
        }
        paths.sort();
        let mut claimed = HashSet::new();
        for path in paths.iter().filter(|p| extension(p) == "cue") {
            if cancelled() {
                bail!("Scan cancelled")
            }
            match disc_album(path, &library.id, &self.preferences) {
                Ok((album, files)) => {
                    claimed.extend(files);
                    library.albums.push(album);
                }
                Err(e) => library.warnings.push(format!("{}: {e:#}", path.display())),
            }
        }
        let mut groups: BTreeMap<(PathBuf, String, String), Album> = BTreeMap::new();
        for (i, path) in paths.iter().enumerate() {
            if cancelled() {
                bail!("Scan cancelled")
            }
            if i % 32 == 0 {
                progress(i);
            }
            if claimed.contains(path)
                || !matches!(
                    extension(path).as_str(),
                    "flac" | "mp3" | "wav" | "aiff" | "aif" | "ogg" | "m4a" | "aac" | "opus"
                )
            {
                continue;
            }
            match tagged_track(path, &library.id) {
                Ok((title, artist, track)) => {
                    let parent = path.parent().unwrap().to_owned();
                    let key = (parent.clone(), title.clone(), artist.clone());
                    let album = groups.entry(key).or_insert_with(|| Album {
                        id: id(
                            &library.id,
                            format!("{}\n{title}\n{artist}", parent.display()),
                        ),
                        title,
                        artist,
                        tracks: vec![],
                        source_label: parent.display().to_string(),
                    });
                    album.tracks.push(track);
                }
                Err(e) => library.warnings.push(format!("{}: {e:#}", path.display())),
            }
        }
        for mut album in groups.into_values() {
            album.tracks.sort_by(|a, b| {
                (a.disc, a.number, &a.id.item).cmp(&(b.disc, b.number, &b.id.item))
            });
            library.albums.push(album);
        }
        library.albums.sort_by(|a, b| {
            (&a.artist, &a.title, &a.id.item).cmp(&(&b.artist, &b.title, &b.id.item))
        });
        Ok(library)
    }
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
fn tagged_track(path: &Path, library: &str) -> Result<(String, String, Track)> {
    let file = Probe::open(path)?.read()?;
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let folder = path
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let title = tag
        .and_then(|t| t.album())
        .map(|s| s.into_owned())
        .unwrap_or(folder);
    let artist = tag
        .and_then(|t| {
            t.get_string(ItemKey::AlbumArtist)
                .map(str::to_owned)
                .or_else(|| t.artist().map(|s| s.into_owned()))
        })
        .unwrap_or_else(|| "Unknown artist".into());
    let locator = path
        .to_str()
        .context("Track path must be valid UTF-8")?
        .to_owned();
    let track = Track {
        id: id(library, locator.clone()),
        title: tag
            .and_then(|t| t.title())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
        artist: tag
            .and_then(|t| t.artist())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| artist.clone()),
        number: tag.and_then(|t| t.track()).unwrap_or(0),
        disc: tag.and_then(|t| t.disk()).unwrap_or(1),
        duration_seconds: file.properties().duration().as_secs_f64(),
        access: MediaAccess::File { locator },
    };
    Ok((title, artist, track))
}
fn disc_album(
    path: &Path,
    library: &str,
    preferences: &Preferences,
) -> Result<(Album, Vec<PathBuf>)> {
    let text = std::fs::read_to_string(path)?;
    let sheet = cue::parse_cue(&text)?;
    let parent = path.parent().unwrap();
    let files = sheet
        .files
        .iter()
        .map(|f| cue::resolve_local_file(parent, &f.filename))
        .collect::<Result<Vec<_>, _>>()?;
    let layouts = cue::read_cue_layout(path)?;
    let sidecars = redumper::find_sidecars(path);
    let raw_present = sidecars.scram.is_some();
    let raw_valid = raw_present && redumper::validate_current_cd_raw(&sidecars).is_ok();
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
    let title = block
        .and_then(|b| b.title.clone())
        .or_else(|| field("TITLE "))
        .unwrap_or_else(|| {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let artist = block
        .and_then(|b| b.performer.clone())
        .or_else(|| field("PERFORMER "))
        .unwrap_or_else(|| "Unknown artist".into());
    let cue_path = path
        .to_str()
        .context("CUE path must be valid UTF-8")?
        .to_owned();
    let tracks = layouts
        .iter()
        .filter(|l| l.kind == TrackKind::Audio)
        .map(|l| Track {
            id: id(library, format!("{cue_path}#{}", l.number)),
            title: format!("Track {}", l.number),
            artist: artist.clone(),
            number: l.number as u32,
            disc: 1,
            duration_seconds: l.length_sectors as f64 / 75.0,
            access: if raw_valid && preferences.dump_preference == DumpPreference::Redumper {
                MediaAccess::Managed {
                    provider: "redumper-pending".into(),
                    item: cue_path.clone(),
                }
            } else {
                MediaAccess::DiscTrack {
                    cue: cue_path.clone(),
                    number: l.number,
                }
            },
        })
        .collect();
    let label = if raw_valid && preferences.dump_preference == DumpPreference::Redumper {
        "Redumper raw • decoder pending; select BIN/CUE in Settings and reopen"
    } else if raw_present && !raw_valid {
        "BIN/CUE • incomplete or invalid raw sidecars"
    } else {
        "BIN/CUE"
    };
    Ok((
        Album {
            id: id(library, cue_path),
            title,
            artist,
            tracks,
            source_label: label.into(),
        },
        files,
    ))
}

pub fn demo_library() -> Library {
    Library {
        id: "demo".into(),
        root: "Layout demo — generated albums, no audio".into(),
        albums: (0..2000)
            .map(|i| Album {
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
    fn preference_roundtrip_and_library_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
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
                id: "a".into(),
                ..Library::default()
            })
            .unwrap();
        store
            .save_library(&Library {
                id: "b".into(),
                ..Library::default()
            })
            .unwrap();
        assert_eq!(store.library().unwrap().unwrap().id, "b");
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
}
