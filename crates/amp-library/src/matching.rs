//! Explicit local-folder matching. Shared libraries own audio/protocol/assignment
//! mechanisms; this adapter owns read-only files, cache and local name authority.
use crate::Store;
use amp_core::{Album, MediaAccess};
use anyhow::{Context, Result, bail};
use junk_libs_music::fingerprint::{self, AudioFingerprint, ByteSource, MediaHint};
use junk_libs_music::{LocalTrack, matching::AlbumMatch, provider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    pub album: Album,
    pub matches: Vec<AlbumMatch>,
    pub truncated: bool,
    pub file_digests: BTreeMap<String, String>,
    pub covers: BTreeMap<String, String>,
    pub artwork_error: Option<String>,
}
static NEXT_ACOUSTID: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
const USER_AGENT: &str = concat!(
    "TrashAmp/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/AberrantWolf/trash-amp)"
);
fn checkpoint(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        bail!("Album matching cancelled");
    }
    Ok(())
}
fn cache(store: &Store, key: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(store
        .0
        .query_row(
            "SELECT value FROM metadata_http WHERE key=? AND expires>?",
            rusqlite::params![key, now()],
            |r| r.get(0),
        )
        .optional()?)
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn save(store: &Store, key: &str, value: &str) -> Result<()> {
    store.0.execute("INSERT INTO metadata_http(key,value,expires) VALUES (?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,expires=excluded.expires",rusqlite::params![key,value,now()+7*86400])?;
    Ok(())
}
fn hash(path: &str, cancelled: &dyn Fn() -> bool) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    if !f.metadata()?.is_file() || f.metadata()?.len() > 64 * 1024 * 1024 * 1024 {
        bail!("Unsupported file or file exceeds 64 GiB");
    }
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    let mut count = 0u64;
    loop {
        checkpoint(cancelled)?;
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > 64 * 1024 * 1024 * 1024 {
            bail!("File grew beyond bound");
        }
        h.update(&b[..n]);
    }
    checkpoint(cancelled)?;
    Ok(format!("{:x}", h.finalize()))
}
struct Reader {
    file: std::fs::File,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl Read for Reader {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if (self.stop)() {
            return Err(std::io::Error::other("matching cancelled"));
        }
        self.file.read(b)
    }
}
impl Seek for Reader {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        if (self.stop)() {
            return Err(std::io::Error::other("matching cancelled"));
        }
        self.file.seek(p)
    }
}
struct Http {
    agent: ureq::Agent,
}
trait Transport {
    fn request(
        &self,
        store: &Store,
        url: url::Url,
        form: Option<&[(&str, String)]>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>>;
    fn cover(&self, id: &str, root: &Path, cancelled: &dyn Fn() -> bool) -> Result<Option<String>>;
}
impl Http {
    fn new() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(12)))
                .http_status_as_error(false)
                .max_redirects(0)
                .user_agent(USER_AGENT)
                .build()
                .into(),
        }
    }
}
impl Transport for Http {
    fn request(
        &self,
        store: &Store,
        url: url::Url,
        form: Option<&[(&str, String)]>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>> {
        let key = format!(
            "digital-response:v1:{url}:{}",
            form.map(|v| v
                .iter()
                .filter(|(k, _)| *k != "client")
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<String>())
                .unwrap_or_default()
        );
        if let Some(value) = cache(store, &key)? {
            return Ok(value.into_bytes());
        }
        let acoustid = url.host_str() == Some("api.acoustid.org");
        let gate = if acoustid {
            &*NEXT_ACOUSTID
        } else {
            &*crate::metadata::NEXT_REQUEST
        };
        let mut next = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Provider limiter poisoned"))?;
        while Instant::now() < *next {
            checkpoint(cancelled)?;
            std::thread::sleep(Duration::from_millis(30));
        }
        checkpoint(cancelled)?;
        let response = if let Some(form) = form {
            self.agent
                .post(url.as_str())
                .send_form(form.iter().map(|(k, v)| (*k, v.as_str())))
        } else {
            self.agent.get(url.as_str()).call()
        };
        *next = Instant::now() + Duration::from_millis(if acoustid { 350 } else { 1100 });
        let mut response = response?;
        let status = response.status().as_u16();
        if status == 429 || status >= 500 {
            *next = Instant::now() + Duration::from_secs(60);
        }
        drop(next);
        if status == 404 {
            return Ok(if acoustid {
                br#"{"status":"ok","results":[]}"#.to_vec()
            } else {
                br#"{"releases":[]}"#.to_vec()
            });
        }
        if status != 200 {
            bail!("Provider HTTP {status}; album matching stopped");
        }
        let mut bytes = Vec::new();
        let mut body = response.body_mut().as_reader();
        let mut buf = [0; 32768];
        loop {
            checkpoint(cancelled)?;
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if bytes.len() + n
                > if acoustid {
                    1024 * 1024
                } else {
                    8 * 1024 * 1024
                }
            {
                bail!("Provider response exceeds 8 MiB");
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        checkpoint(cancelled)?;
        // Refusals and malformed entities must not poison the response cache.
        if acoustid {
            provider::parse_acoustid(&bytes)?;
        } else {
            provider::parse_releases(
                &bytes,
                url.path()
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .is_some_and(junk_libs_music::valid_mbid),
            )?;
        }
        save(store, &key, std::str::from_utf8(&bytes)?)?;
        Ok(bytes)
    }
    fn cover(&self, id: &str, root: &Path, cancelled: &dyn Fn() -> bool) -> Result<Option<String>> {
        if !junk_libs_music::valid_mbid(id) {
            bail!("Invalid release ID");
        }
        let target = root.join(format!("{id}.image"));
        if target.is_file() {
            return Ok(Some(target.to_string_lossy().into()));
        }
        let mut url = url::Url::parse(&format!(
            "https://coverartarchive.org/release/{id}/front-250"
        ))?;
        for _ in 0..4 {
            checkpoint(cancelled)?;
            let host = url.host_str().unwrap_or_default();
            if url.scheme() != "https"
                || url.port_or_known_default() != Some(443)
                || !url.username().is_empty()
                || url.password().is_some()
                || !(host == "coverartarchive.org"
                    || host == "archive.org"
                    || host.ends_with(".archive.org"))
            {
                bail!("Artwork redirect left allowed origins");
            }
            let mut response = self.agent.get(url.as_str()).call()?;
            let code = response.status().as_u16();
            if code == 404 {
                return Ok(None);
            }
            if (300..400).contains(&code) {
                let location = response
                    .headers()
                    .get("location")
                    .context("Artwork redirect without location")?
                    .to_str()?;
                url = url.join(location)?;
                continue;
            }
            if code != 200 {
                bail!("Artwork HTTP {code}");
            }
            let mut bytes = Vec::new();
            let mut body = response.body_mut().as_reader();
            let mut buf = [0; 32768];
            loop {
                checkpoint(cancelled)?;
                let n = body.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if bytes.len() + n > 1024 * 1024 {
                    bail!("Cover preview exceeds 1 MiB");
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            checkpoint(cancelled)?;
            let mut reader =
                image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(8192);
            limits.max_image_height = Some(8192);
            limits.max_alloc = Some(64 * 1024 * 1024);
            reader.limits(limits);
            let thumbnail = reader.decode()?.thumbnail(250, 250);
            checkpoint(cancelled)?;
            std::fs::create_dir_all(root)?;
            let partial = target.with_extension("partial");
            thumbnail.save_with_format(&partial, image::ImageFormat::Png)?;
            checkpoint(cancelled)?;
            std::fs::rename(partial, &target)?;
            return Ok(Some(target.to_string_lossy().into()));
        }
        bail!("Artwork redirect limit exceeded")
    }
}
pub fn discover(
    album: &Album,
    key: &str,
    store: &Store,
    cover_root: &Path,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Review> {
    discover_with(album, key, store, cover_root, stop, &Http::new())
}
fn discover_with(
    album: &Album,
    key: &str,
    store: &Store,
    cover_root: &Path,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
    http: &dyn Transport,
) -> Result<Review> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("Set an AcoustID application key in Settings before matching local albums");
    }
    if album.tracks.is_empty() || album.tracks.len() > junk_libs_music::MAX_TRACKS {
        bail!("Choose an album with 1–256 FLAC/MP3 files");
    }
    let mut local = Vec::new();
    let mut file_digests = BTreeMap::new();
    let mut recordings = BTreeSet::new();
    for track in &album.tracks {
        checkpoint(&*stop)?;
        let MediaAccess::File { locator } = &track.access else {
            bail!("Local album matching requires file tracks");
        };
        let digest = hash(locator, &*stop)?;
        let cache_key = format!(
            "digital-fingerprint:v1:{digest}:{}",
            fingerprint::IMPLEMENTATION
        );
        let audio: AudioFingerprint = if let Some(value) = cache(store, &cache_key)? {
            let a: AudioFingerprint = serde_json::from_str(&value)?;
            a.validate()?;
            a
        } else {
            let file = std::fs::File::open(locator)?;
            let length = file.metadata()?.len();
            let ext = Path::new(locator).extension().and_then(|s| s.to_str());
            let reader = Reader {
                file,
                stop: stop.clone(),
            };
            let audio = fingerprint::compute(
                ByteSource::new(reader, MediaHint::new(Some(length), ext, None)),
                &mut || {
                    if stop() {
                        Err(junk_libs_music::Error::Cancelled)
                    } else {
                        Ok(())
                    }
                },
            )?;
            if hash(locator, &*stop)? != digest {
                bail!("Source file changed during matching");
            }
            save(store, &cache_key, &serde_json::to_string(&audio)?)?;
            audio
        };
        file_digests.insert(track.id.item.clone(), digest);
        if !audio.usable() {
            local.push(LocalTrack {
                key: track.id.item.clone(),
                duration_ms: audio.duration_ms,
                disc: None,
                position: None,
                recordings: vec![],
            });
            continue;
        }
        let bytes = http.request(
            store,
            url::Url::parse("https://api.acoustid.org/v2/lookup")?,
            Some(&[
                ("client", key.into()),
                ("duration", audio.duration_ms.div_ceil(1000).to_string()),
                ("fingerprint", audio.fingerprint),
                ("meta", "recordings".into()),
                ("format", "json".into()),
            ]),
            &*stop,
        )?;
        let found = provider::parse_acoustid(&bytes)?;
        for r in &found {
            if r.score >= 0.5 {
                recordings.insert(r.recording_id.clone());
            }
        }
        local.push(LocalTrack {
            key: track.id.item.clone(),
            duration_ms: audio.duration_ms,
            disc: u16::try_from(track.disc).ok().filter(|&n| n > 0),
            position: u16::try_from(track.number).ok().filter(|&n| n > 0),
            recordings: found,
        });
    }
    let mut ids = BTreeMap::<String, usize>::new();
    let mut truncated = recordings.len() > 32;
    for id in recordings.into_iter().take(32) {
        let bytes = http.request(
            store,
            provider::recording_releases_url(&id, 0)?,
            None,
            &*stop,
        )?;
        let page = provider::parse_releases(&bytes, false)?;
        truncated |= page.total.is_some_and(|n| n as usize > page.releases.len());
        for r in page.releases {
            *ids.entry(r.id).or_default() += 1;
        }
    }
    truncated |= ids.len() > 32;
    let mut releases = Vec::new();
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_by_key(|(id, hits)| (std::cmp::Reverse(*hits), id.clone()));
    for (id, _) in ids.into_iter().take(32) {
        let bytes = http.request(store, provider::release_url(&id)?, None, &*stop)?;
        let page = provider::parse_releases(&bytes, true)?;
        if page.releases.first().is_none_or(|r| r.id != id) {
            bail!("Provider returned another release");
        }
        if page.releases[0]
            .media
            .iter()
            .map(|m| m.tracks.len())
            .sum::<usize>()
            > junk_libs_music::MAX_TRACKS
        {
            truncated = true;
            continue;
        }
        releases.extend(page.releases);
    }
    let matches = junk_libs_music::matching::rank(&local, &releases, &mut || {
        if stop() {
            Err(junk_libs_music::Error::Cancelled)
        } else {
            Ok(())
        }
    })?;
    let mut covers = BTreeMap::new();
    let mut artwork_error = None;
    for candidate in matches.iter().take(8) {
        match http.cover(&candidate.release.id, cover_root, &*stop) {
            Ok(Some(path)) => {
                covers.insert(candidate.release.id.clone(), path);
            }
            Ok(None) => {}
            Err(e) => {
                checkpoint(&*stop)?;
                artwork_error = Some(e.to_string());
                break;
            }
        }
    }
    checkpoint(&*stop)?;
    Ok(Review {
        album: album.clone(),
        matches,
        truncated,
        file_digests,
        covers,
        artwork_error,
    })
}
pub fn apply(review: &Review, release_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Album> {
    let choice = review
        .matches
        .iter()
        .find(|m| m.release.id == release_id)
        .context("Unknown album choice")?;
    if !choice.complete_tracklist
        || choice.ambiguous_mapping
        || choice.duration_conflicts > 0
        || choice.mappings.len() != review.album.tracks.len()
        || choice.release_tracks != review.album.tracks.len()
    {
        bail!("This release needs manual track mapping");
    }
    let mut album = review.album.clone();
    let mut names = Vec::new();
    let mut seen_files = BTreeSet::new();
    let mut seen_slots = BTreeSet::new();
    for (index, track) in album.tracks.iter().enumerate() {
        let MediaAccess::File { locator } = &track.access else {
            bail!("Non-file track");
        };
        if Some(&hash(locator, cancelled)?) != review.file_digests.get(&track.id.item) {
            bail!("Audio changed since matching; search again");
        }
        let map = choice
            .mappings
            .iter()
            .find(|m| m.key == track.id.item)
            .context("Missing track mapping")?;
        if !seen_files.insert(&track.id.item) || !seen_slots.insert((map.medium, map.position)) {
            bail!("The reviewed mapping is not one-to-one; search again");
        }
        let remote = choice
            .release
            .media
            .iter()
            .find(|m| m.position == Some(map.medium))
            .and_then(|m| m.tracks.iter().find(|t| t.position == map.position))
            .context("Invalid track mapping")?;
        if remote.recording_id.as_deref() != Some(&map.recording_id) {
            bail!("Inconsistent recording mapping");
        }
        names.push(crate::metadata::TrackNames {
            disc: 1,
            number: index as u32 + 1,
            title: remote.title.clone(),
            artist: if remote.artist.is_empty() {
                choice.release.artist.clone()
            } else {
                remote.artist.clone()
            },
        });
    }
    checkpoint(cancelled)?;
    // Temporary unique slots keep duplicate source tag numbers from aliasing title mappings.
    for (index, track) in album.tracks.iter_mut().enumerate() {
        track.disc = 1;
        track.number = index as u32 + 1;
    }
    crate::metadata::apply(
        &mut album,
        &crate::metadata::Names {
            title: choice.release.title.clone(),
            artist: choice.release.artist.clone(),
            tracks: names,
        },
    );
    for track in &mut album.tracks {
        let map = choice
            .mappings
            .iter()
            .find(|m| m.key == track.id.item)
            .context("Missing mapping")?;
        track.disc = u32::from(map.medium);
        track.number = u32::from(map.position);
    }
    album.tracks.sort_by_key(|t| (t.disc, t.number));
    Ok(album)
}

/// One retained accepted source per local album; no per-lookup history.
pub fn remember(store: &Store, review: &Review, release: &str) -> Result<()> {
    let key = format!(
        "digital-accepted:{}",
        serde_json::to_string(&review.album.id)?
    );
    store.0.execute("INSERT INTO metadata_matches(album_id,value) VALUES (?1,?2) ON CONFLICT(album_id) DO UPDATE SET value=excluded.value",rusqlite::params![key,serde_json::to_string(&(accepted_review(review, release)?,release))?])?;
    Ok(())
}
pub(crate) fn restore(
    library: &mut amp_core::Library,
    store: &Store,
    cancelled: &dyn Fn() -> bool,
    update: &mut dyn FnMut(&Album),
) -> Result<()> {
    use rusqlite::OptionalExtension;
    if library
        .scan
        .as_ref()
        .is_some_and(|scan| scan.source_disconnected)
    {
        return Ok(());
    }
    for album in &mut library.albums {
        checkpoint(cancelled)?;
        if album.tracks.iter().any(|track| track.scan_status.is_some()) {
            continue;
        }
        let key = format!("digital-accepted:{}", serde_json::to_string(&album.id)?);
        let saved: Option<String> = store
            .0
            .query_row(
                "SELECT value FROM metadata_matches WHERE album_id=?",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(saved) = saved {
            let (mut review, release): (Review, String) = serde_json::from_str(&saved)?;
            let identities = |album: &Album| {
                album
                    .tracks
                    .iter()
                    .map(|track| {
                        (
                            track.id.library.clone(),
                            track.id.provider.clone(),
                            track.id.item.clone(),
                        )
                    })
                    .collect::<BTreeSet<_>>()
            };
            let current = identities(album);
            if current.len() != album.tracks.len()
                || current != identities(&review.album)
                || album.tracks.len() != review.album.tracks.len()
            {
                continue;
            }
            review.album = album.clone();
            if let Ok(next) = apply(&review, &release, cancelled) {
                *album = next;
                update(album);
            } else {
                checkpoint(cancelled)?;
            }
        }
    }
    Ok(())
}

fn accepted_review(review: &Review, release: &str) -> Result<Review> {
    let mut accepted = review.clone();
    let candidate = accepted
        .matches
        .iter()
        .find(|m| m.release.id == release)
        .cloned()
        .context("Unknown accepted release")?;
    accepted.matches = vec![candidate];
    accepted.covers.retain(|id, _| id == release);
    Ok(accepted)
}
pub fn saved_artwork(
    store: &Store,
    library: &amp_core::Library,
) -> Result<BTreeMap<String, String>> {
    use rusqlite::OptionalExtension;
    let mut result = BTreeMap::new();
    for album in &library.albums {
        let key = format!("digital-accepted:{}", serde_json::to_string(&album.id)?);
        let saved: Option<String> = store
            .0
            .query_row(
                "SELECT value FROM metadata_matches WHERE album_id=?",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(saved) = saved {
            let (review, release): (Review, String) = serde_json::from_str(&saved)?;
            if let Some(path) = review.covers.get(&release) {
                result.insert(album.id.item.clone(), path.clone());
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    const RELEASE: &str = "00000000-0000-4000-8000-000000000090";
    fn id(n: usize) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }
    struct Fixture {
        fingerprints: Mutex<BTreeMap<String, usize>>,
        requests: AtomicUsize,
    }
    impl Transport for Fixture {
        fn request(
            &self,
            _: &Store,
            url: url::Url,
            form: Option<&[(&str, String)]>,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<Vec<u8>> {
            checkpoint(cancelled)?;
            self.requests.fetch_add(1, Ordering::SeqCst);
            let value = if let Some(form) = form {
                let fingerprint = &form
                    .iter()
                    .find(|(key, _)| *key == "fingerprint")
                    .unwrap()
                    .1;
                let mut values = self.fingerprints.lock().unwrap();
                let next = values.len() + 1;
                let n = *values.entry(fingerprint.clone()).or_insert(next);
                serde_json::json!({"status":"ok","results":[{"score":0.99,"recordings":[{"id":id(n)}]}]})
            } else if url.query_pairs().any(|(key, _)| key == "recording") {
                serde_json::json!({"release-count":1,"releases":[{"id":RELEASE,"title":"Matched album"}]})
            } else {
                serde_json::json!({"id":RELEASE,"title":"Matched album","artist-credit":[{"name":"Matched artist","artist":{"id":id(99),"name":"Matched artist"}}],"media":[{"id":id(98),"position":1,"format":"Digital Media","track-count":2,"tracks":[{"id":id(97),"position":1,"number":"1","title":"Matched first","length":12000,"recording":{"id":id(1),"title":"Matched first","length":12000}},{"id":id(96),"position":2,"number":"2","title":"Matched second","length":12000,"recording":{"id":id(2),"title":"Matched second","length":12000}}]}]})
            };
            Ok(serde_json::to_vec(&value)?)
        }
        fn cover(&self, _: &str, _: &Path, _: &dyn Fn() -> bool) -> Result<Option<String>> {
            Ok(None)
        }
    }
    fn fixture() -> (tempfile::TempDir, Store, Album, Fixture) {
        let root = tempfile::tempdir().unwrap();
        let mut tracks = Vec::new();
        let mut metadata = amp_core::AlbumMetadata::default();
        for (name, bytes) in [
            (
                "01.flac",
                include_bytes!("../tests/fixtures/digital-matching/01.flac").as_slice(),
            ),
            (
                "02.mp3",
                include_bytes!("../tests/fixtures/digital-matching/02.mp3").as_slice(),
            ),
        ] {
            let path = root.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let mut t = crate::local_file_track(&path, "matching-test").unwrap();
            t.disc = 1;
            t.number = tracks.len() as u32 + 1;
            metadata.tracks.insert(
                t.id.item.clone(),
                amp_core::NameOrigins {
                    title: amp_core::NameOrigin::Generated(t.title.clone()),
                    artist: amp_core::NameOrigin::Generated(t.artist.clone()),
                },
            );
            tracks.push(t);
        }
        metadata.names = amp_core::NameOrigins {
            title: amp_core::NameOrigin::Generated("Folder".into()),
            artist: amp_core::NameOrigin::Generated("Unknown artist".into()),
        };
        let album = Album {
            id: amp_core::ItemId {
                library: "local".into(),
                provider: "filesystem".into(),
                item: "album".into(),
            },
            title: "Folder".into(),
            artist: "Unknown artist".into(),
            tracks,
            source_label: "Local".into(),
            metadata,
        };
        let store = Store::open(&root.path().join("cache.sqlite")).unwrap();
        (
            root,
            store,
            album,
            Fixture {
                fingerprints: Default::default(),
                requests: 0.into(),
            },
        )
    }
    #[test]
    fn real_local_audio_discovery_review_apply_and_offline_refresh_preserve_sources() {
        let (root, store, album, http) = fixture();
        let review = discover_with(
            &album,
            "fixture-key",
            &store,
            root.path(),
            Arc::new(|| false),
            &http,
        )
        .unwrap();
        assert_eq!(review.matches.len(), 1);
        assert_eq!(review.matches[0].mappings.len(), 2);
        assert_eq!(http.fingerprints.lock().unwrap().len(), 2);
        let applied = apply(&review, RELEASE, &|| false).unwrap();
        assert_eq!(applied.title, "Matched album");
        assert_eq!(applied.tracks[0].title, "Matched first");
        assert_eq!(applied.tracks[1].title, "Matched second");
        remember(&store, &review, RELEASE).unwrap();
        let mut library = amp_core::Library {
            albums: vec![album.clone()],
            ..Default::default()
        };
        restore(&mut library, &store, &|| false, &mut |_| {}).unwrap();
        assert_eq!(library.albums[0].tracks[1].title, "Matched second");
        for t in &album.tracks {
            let MediaAccess::File { locator } = &t.access else {
                panic!()
            };
            assert_eq!(
                hash(locator, &|| false).unwrap(),
                review.file_digests[&t.id.item]
            );
        }
    }
    #[test]
    fn manual_names_survive_selection_and_changed_bytes_refuse_apply() {
        let (root, store, mut album, http) = fixture();
        album.title = "My correction".into();
        album.metadata.names.title = amp_core::NameOrigin::Source;
        album.tracks[0].title = "My track".into();
        album
            .metadata
            .tracks
            .get_mut(&album.tracks[0].id.item)
            .unwrap()
            .title = amp_core::NameOrigin::Source;
        let review = discover_with(
            &album,
            "fixture-key",
            &store,
            root.path(),
            Arc::new(|| false),
            &http,
        )
        .unwrap();
        let applied = apply(&review, RELEASE, &|| false).unwrap();
        assert_eq!(applied.title, "My correction");
        assert_eq!(applied.tracks[0].title, "My track");
        let MediaAccess::File { locator } = &album.tracks[0].access else {
            panic!()
        };
        std::fs::write(locator, b"changed bytes").unwrap();
        assert!(
            apply(&review, RELEASE, &|| false)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
    }
    #[test]
    fn accepted_mapping_restores_after_track_order_changes() {
        let (root, store, mut album, http) = fixture();
        let review = discover_with(
            &album,
            "fixture-key",
            &store,
            root.path(),
            Arc::new(|| false),
            &http,
        )
        .unwrap();
        remember(&store, &review, RELEASE).unwrap();
        album.tracks.reverse();
        let mut library = amp_core::Library {
            albums: vec![album],
            ..Default::default()
        };
        restore(&mut library, &store, &|| false, &mut |_| {}).unwrap();
        assert_eq!(library.albums[0].title, "Matched album");
        assert_eq!(library.albums[0].tracks[0].title, "Matched first");
        assert_eq!(library.albums[0].tracks[1].title, "Matched second");
    }
    #[test]
    fn duplicate_source_track_numbers_keep_independent_fingerprint_mappings() {
        let (root, store, mut album, http) = fixture();
        album.tracks[1].number = album.tracks[0].number;
        let review = discover_with(
            &album,
            "fixture-key",
            &store,
            root.path(),
            Arc::new(|| false),
            &http,
        )
        .unwrap();
        let applied = apply(&review, RELEASE, &|| false).unwrap();
        assert_eq!(applied.tracks[0].title, "Matched first");
        assert_eq!(applied.tracks[1].title, "Matched second");
        assert_eq!(applied.tracks[1].number, 2);
    }
    #[test]
    fn inconsistent_completed_mapping_cannot_apply_two_files_to_one_release_slot() {
        let (root, store, album, http) = fixture();
        let mut review = discover_with(
            &album,
            "fixture-key",
            &store,
            root.path(),
            Arc::new(|| false),
            &http,
        )
        .unwrap();
        let first = review.matches[0].mappings[0].clone();
        let second = &mut review.matches[0].mappings[1];
        second.medium = first.medium;
        second.position = first.position;
        second.recording_id = first.recording_id;
        assert!(
            apply(&review, RELEASE, &|| false)
                .unwrap_err()
                .to_string()
                .contains("one-to-one")
        );
    }
    #[test]
    fn incomplete_refresh_does_not_restore_names_from_unverified_sources() {
        let (root, store, album, http) = fixture();
        let review = discover_with(
            &album,
            "fixture-key",
            &store,
            root.path(),
            Arc::new(|| false),
            &http,
        )
        .unwrap();
        remember(&store, &review, RELEASE).unwrap();
        let mut library = amp_core::Library {
            albums: vec![album.clone()],
            ..Default::default()
        };
        library.albums[0].tracks[0].scan_status = Some(amp_core::ScanStatus::Unverified {
            path: "offline mount".into(),
            reason: "refresh incomplete".into(),
        });
        restore(&mut library, &store, &|| false, &mut |_| {
            panic!("uncertain source must not enrich")
        })
        .unwrap();
        assert_eq!(library.albums[0].title, album.title);
        library.albums[0].tracks[0].scan_status = None;
        library.scan = Some(amp_core::ScanSummary {
            source_disconnected: true,
            ..Default::default()
        });
        restore(&mut library, &store, &|| false, &mut |_| {
            panic!("disconnected source must not enrich")
        })
        .unwrap();
        assert_eq!(library.albums[0].title, album.title);
    }
    #[test]
    fn cancelled_local_discovery_sends_no_requests() {
        let (root, store, album, http) = fixture();
        assert!(
            discover_with(
                &album,
                "fixture-key",
                &store,
                root.path(),
                Arc::new(|| true),
                &http
            )
            .is_err()
        );
        assert_eq!(http.requests.load(Ordering::SeqCst), 0);
    }
}
