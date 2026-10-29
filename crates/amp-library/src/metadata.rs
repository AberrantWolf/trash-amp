//! Keyless, read-only MusicBrainz enrichment. Network work belongs on a worker thread.
//! Local metadata wins; ambiguous releases are recorded but never chosen arbitrarily.
use crate::{Store, normalized};
use amp_core::{Album, Library, MediaAccess, NameOrigin, Track};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use junk_libs_disc::{cue_audio::CueAudioDisc, layout::TrackKind, redumper};
use lofty::{file::TaggedFileExt, prelude::ItemKey, probe::Probe};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const BASE: &str = "https://musicbrainz.org/ws/2";
const USER_AGENT: &str = concat!(
    "TrashAmp/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/AberrantWolf)"
);
static NEXT_REQUEST: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
const WEEK: i64 = 7 * 86400;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Query {
    Disc(String),
    Release(String),
    Search { title: String, artist: String },
}
impl Query {
    fn key(&self) -> String {
        format!("musicbrainz:v1:{}", serde_json::to_string(self).unwrap())
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Calculate the documented MusicBrainz ID from absolute CD sector offsets (LBA + 150).
/// Only complete, single-session audio TOCs beginning at track 1 are accepted here.
pub fn disc_id(offsets: &[u32], leadout: u32) -> Result<String> {
    if offsets.is_empty()
        || offsets.len() > 99
        || offsets[0] < 150
        || offsets.windows(2).any(|p| p[0] >= p[1])
        || leadout <= *offsets.last().unwrap()
    {
        bail!("Invalid audio TOC for MusicBrainz");
    }
    let mut hex = format!("01{:02X}{leadout:08X}", offsets.len());
    for i in 0..99 {
        hex.push_str(&format!("{:08X}", offsets.get(i).copied().unwrap_or(0)));
    }
    Ok(STANDARD
        .encode(Sha1::digest(hex.as_bytes()))
        .replace('+', ".")
        .replace('/', "_")
        .replace('=', "-"))
}
fn mbid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn query_for(album: &Album) -> Result<Option<Query>> {
    let Some(first) = album.tracks.first() else {
        return Ok(None);
    };
    // Fully named file albums have nothing to fill in this metadata slice.
    if album.tracks.iter().all(|t| {
        matches!(t.access, MediaAccess::File { .. })
            && !missing_track_title(album, t)
            && !missing_track_artist(album, t)
    }) && !missing(&album.metadata.names.title, &album.title)
        && !missing(&album.metadata.names.artist, &album.artist)
    {
        return Ok(None);
    }
    let frames = match &first.access {
        MediaAccess::RedumperTrack { anchor, .. } => {
            let layout =
                redumper::pcm::read_audio_layout(&redumper::find_sidecars(Path::new(anchor)))?;
            if layout
                .iter()
                .enumerate()
                .any(|(i, t)| t.number as usize != i + 1)
            {
                return Ok(None);
            }
            let to_toc_sector = |frame: i64| {
                frame
                    .checked_div(588)
                    .and_then(|sector| sector.checked_add(150))
                    .and_then(|sector| u64::try_from(sector).ok())
            };
            let Some(offsets) = layout
                .iter()
                .map(|track| to_toc_sector(track.start_frame))
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(None);
            };
            let Some(leadout) = to_toc_sector(layout.last().unwrap().end_frame) else {
                return Ok(None);
            };
            Some((offsets, leadout))
        }
        MediaAccess::DiscTrack { cue, .. } => {
            let disc = CueAudioDisc::open(Path::new(cue))?;
            if disc
                .tracks
                .iter()
                .enumerate()
                .any(|(i, t)| t.kind != TrackKind::Audio || t.number as usize != i + 1)
            {
                return Ok(None);
            }
            // A synthetic initial pregap precedes LBA 0; a stored first-track prefix does not.
            let initial = disc.tracks[0].synthetic_pregap_frames;
            let offset = |f: u64| (f - initial) / 588 + 150;
            Some((
                disc.tracks
                    .iter()
                    .map(|t| offset(t.start_frame))
                    .collect::<Vec<_>>(),
                offset(disc.tracks.last().unwrap().end_frame),
            ))
        }
        MediaAccess::File { locator } => {
            let file = Probe::open(locator)?
                .options(
                    lofty::config::ParseOptions::new()
                        .read_cover_art(false)
                        .read_properties(false),
                )
                .read()?;
            if let Some(id) = file
                .tags()
                .iter()
                .find_map(|t| t.get_string(ItemKey::MusicBrainzReleaseId))
                && mbid(id)
            {
                return Ok(Some(Query::Release(id.to_owned())));
            }
            None
        }
        MediaAccess::Managed { .. } => return Ok(None),
    };
    if let Some((offsets, end)) = frames {
        let offsets = offsets
            .into_iter()
            .map(u32::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Some(Query::Disc(disc_id(&offsets, u32::try_from(end)?)?)));
    }
    if !unknown(&album.title)
        && !unknown(&album.artist)
        && album.artist != "Various artists"
        && album.tracks.iter().all(|t| t.number > 0)
    {
        Ok(Some(Query::Search {
            title: album.title.clone(),
            artist: album.artist.clone(),
        }))
    } else {
        Ok(None)
    }
}

trait Transport {
    fn get(&self, query: &Query, stopped: &dyn Fn() -> bool) -> Result<Value>;
}
pub struct MusicBrainz {
    agent: ureq::Agent,
}
impl Default for MusicBrainz {
    fn default() -> Self {
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
fn quoted(s: &str) -> String {
    format!(
        "\"{}\"",
        s.chars()
            .flat_map(|c| if c.is_ascii_punctuation() {
                vec!['\\', c]
            } else {
                vec![c]
            })
            .collect::<String>()
    )
}
impl Transport for MusicBrainz {
    fn get(&self, query: &Query, stopped: &dyn Fn() -> bool) -> Result<Value> {
        let mut gate = NEXT_REQUEST
            .lock()
            .map_err(|_| anyhow::anyhow!("metadata request lock poisoned"))?;
        while Instant::now() < *gate {
            if stopped() {
                bail!("Metadata lookup cancelled");
            }
            std::thread::sleep(
                Duration::from_millis(50).min(gate.saturating_duration_since(Instant::now())),
            );
        }
        if stopped() {
            bail!("Metadata lookup cancelled");
        }
        *gate = Instant::now() + Duration::from_millis(1100);
        let request = match query {
            Query::Disc(id) => self
                .agent
                .get(&format!("{BASE}/discid/{id}"))
                .query("cdstubs", "no")
                .query("inc", "recordings artist-credits"),
            Query::Release(id) => self
                .agent
                .get(&format!("{BASE}/release/{id}"))
                .query("inc", "recordings artist-credits"),
            Query::Search { title, artist } => self
                .agent
                .get(&format!("{BASE}/release"))
                .query(
                    "query",
                    format!("release:{} AND artist:{}", quoted(title), quoted(artist)),
                )
                .query("limit", "2"),
        };
        // Keep the gate through response headers; overlapping scans share one limiter.
        let response = request
            .query("fmt", "json")
            .header("Accept", "application/json")
            .call();
        let mut response = match response {
            Ok(r) => r,
            Err(e) => {
                *gate = Instant::now() + Duration::from_secs(60);
                return Err(e.into());
            }
        };
        let status = response.status().as_u16();
        if status == 429 || status == 503 {
            let wait = response
                .headers()
                .get("Retry-After")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| {
                    s.parse::<u64>().ok().or_else(|| {
                        httpdate::parse_http_date(s)
                            .ok()
                            .and_then(|d| d.duration_since(SystemTime::now()).ok())
                            .map(|d| d.as_secs())
                    })
                })
                .unwrap_or(60)
                .max(60);
            *gate = Instant::now() + Duration::from_secs(wait.min(86400));
        }
        drop(gate);
        if status == 404 {
            return Ok(json!({"releases":[]}));
        }
        if status != 200 {
            bail!("MusicBrainz HTTP {status}; metadata pass stopped");
        }
        let text = response
            .body_mut()
            .with_config()
            .limit(2 * 1024 * 1024)
            .read_to_string()?;
        let value: Value = serde_json::from_str(&text)?;
        if !value.is_object()
            || (value.get("releases").and_then(Value::as_array).is_none()
                && value.get("media").and_then(Value::as_array).is_none())
        {
            bail!("Unexpected MusicBrainz response");
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrackNames {
    disc: u32,
    number: u32,
    title: String,
    artist: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Names {
    title: String,
    artist: String,
    tracks: Vec<TrackNames>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatchRecord {
    provider: String,
    query: String,
    checked_at: i64,
    status: String,
    releases: Vec<String>,
    names: Option<Names>,
}
fn credit(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|v| {
                    format!(
                        "{}{}",
                        v.get("name")
                            .and_then(Value::as_str)
                            .or_else(|| v.pointer("/artist/name").and_then(Value::as_str))
                            .unwrap_or(""),
                        v.get("joinphrase").and_then(Value::as_str).unwrap_or("")
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}
fn string(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}
fn candidate(album: &Album, release: &Value, query: &Query) -> Option<Names> {
    let title = string(release, "title");
    let artist = credit(&release["artist-credit"]);
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    let media = release["media"].as_array()?;
    let mut names = Vec::new();
    let mut slots = BTreeSet::new();
    for t in &album.tracks {
        if t.number == 0 || !slots.insert((t.disc, t.number)) {
            return None;
        }
        let candidates = media
            .iter()
            .filter(|m| match query {
                Query::Disc(id) => m["discs"]
                    .as_array()
                    .is_some_and(|ds| ds.iter().any(|d| d["id"].as_str() == Some(id))),
                _ => m["position"].as_u64() == Some(t.disc as u64),
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return None;
        }
        let tracks = candidates[0]["tracks"].as_array()?;
        let local_count = album.tracks.iter().filter(|l| l.disc == t.disc).count();
        if tracks.len() != local_count {
            return None;
        }
        let matches = tracks
            .iter()
            .filter(|r| r["position"].as_u64() == Some(t.number as u64))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return None;
        }
        let r = matches[0];
        let rt = r
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| r.pointer("/recording/title").and_then(Value::as_str))?
            .trim();
        let ra = credit(
            r.get("artist-credit")
                .or_else(|| r.pointer("/recording/artist-credit"))
                .unwrap_or(&Value::Null),
        );
        if rt.is_empty() {
            return None;
        }
        // Search matches require complete duration and known-title agreement, not just a name hit.
        if matches!(query, Query::Search { .. }) {
            let length = r
                .get("length")
                .or_else(|| r.pointer("/recording/length"))
                .and_then(Value::as_f64)?
                / 1000.0;
            if !t.duration_seconds.is_finite()
                || t.duration_seconds <= 0.0
                || (length - t.duration_seconds).abs() > 2.0
                || (!missing_track_title(album, t) && normalized(&t.title) != normalized(rt))
            {
                return None;
            }
        }
        names.push(TrackNames {
            disc: t.disc,
            number: t.number,
            title: rt.into(),
            artist: if ra.is_empty() { artist.clone() } else { ra },
        });
    }
    Some(Names {
        title,
        artist,
        tracks: names,
    })
}
fn choose(album: &Album, query: &Query, response: &Value) -> MatchRecord {
    let releases: Vec<&Value> = if response.get("media").is_some() {
        vec![response]
    } else {
        response["releases"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    let mut record = MatchRecord {
        provider: "MusicBrainz".into(),
        query: query.key(),
        checked_at: now(),
        status: "no-match".into(),
        releases: vec![],
        names: None,
    };
    let mut candidates = Vec::new();
    for release in releases {
        if let Some(names) = candidate(album, release, query) {
            record.releases.push(string(release, "id"));
            candidates.push(names);
        }
    }
    if let Some(first) = candidates.first() {
        if candidates.iter().all(|n| n == first) {
            record.status = "matched".into();
            record.names = Some(first.clone());
        } else {
            record.status = "ambiguous".into();
            // Translated track lists must not suppress an agreed album identity.
            // Keep only unanimous fields; never choose a language/edition arbitrarily.
            if candidates
                .iter()
                .all(|n| n.title == first.title && n.artist == first.artist)
            {
                let mut common = first.clone();
                for (i, track) in common.tracks.iter_mut().enumerate() {
                    if !candidates.iter().all(|n| n.tracks[i].title == track.title) {
                        track.title.clear();
                    }
                    if !candidates
                        .iter()
                        .all(|n| n.tracks[i].artist == track.artist)
                    {
                        track.artist.clear();
                    }
                }
                record.names = Some(common);
            }
        }
    }
    record
}
fn unknown(s: &str) -> bool {
    s.trim().is_empty() || normalized(s).starts_with("unknown ")
}
fn missing(origin: &NameOrigin, value: &str) -> bool {
    match origin {
        NameOrigin::Generated(original) => original == value,
        NameOrigin::Unspecified => unknown(value),
        NameOrigin::Source | NameOrigin::MusicBrainz => false,
    }
}
fn missing_track_title(album: &Album, t: &Track) -> bool {
    missing(
        album
            .metadata
            .tracks
            .get(&t.id.item)
            .map(|n| &n.title)
            .unwrap_or(&NameOrigin::Unspecified),
        &t.title,
    )
}
fn missing_track_artist(album: &Album, t: &Track) -> bool {
    missing(
        album
            .metadata
            .tracks
            .get(&t.id.item)
            .map(|n| &n.artist)
            .unwrap_or(&NameOrigin::Unspecified),
        &t.artist,
    )
}
fn apply(album: &mut Album, names: &Names) -> bool {
    let before = serde_json::to_string(album).unwrap();
    if missing(&album.metadata.names.title, &album.title) {
        album.title = names.title.clone();
        album.metadata.names.title = NameOrigin::MusicBrainz;
    }
    if missing(&album.metadata.names.artist, &album.artist) {
        album.artist = names.artist.clone();
        album.metadata.names.artist = NameOrigin::MusicBrainz;
    }
    for t in &mut album.tracks {
        if let Some(n) = names
            .tracks
            .iter()
            .find(|n| n.disc == t.disc && n.number == t.number)
        {
            let origins = album.metadata.tracks.get(&t.id.item);
            let fill_title = !n.title.is_empty()
                && missing(
                    origins
                        .map(|n| &n.title)
                        .unwrap_or(&NameOrigin::Unspecified),
                    &t.title,
                );
            let fill_artist = !n.artist.is_empty()
                && missing(
                    origins
                        .map(|n| &n.artist)
                        .unwrap_or(&NameOrigin::Unspecified),
                    &t.artist,
                );
            if !fill_title && !fill_artist {
                continue;
            }
            let origins = album.metadata.tracks.entry(t.id.item.clone()).or_default();
            if fill_title {
                t.title = n.title.clone();
                origins.title = NameOrigin::MusicBrainz;
            }
            if fill_artist {
                t.artist = n.artist.clone();
                origins.artist = NameOrigin::MusicBrainz;
            }
        }
    }
    let changed = serde_json::to_string(album).unwrap() != before;
    if changed && !album.source_label.contains(" • MusicBrainz") {
        album.source_label.push_str(" • MusicBrainz");
    }
    changed
}

fn cached(store: &Store, query: &Query) -> Result<Option<(Value, i64)>> {
    let row: Option<(String, i64)> = store
        .0
        .query_row(
            "SELECT value,expires FROM metadata_http WHERE key=?",
            [query.key()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(s, e)| Ok((serde_json::from_str(&s)?, e)))
        .transpose()
}
fn response(
    store: &Store,
    transport: &dyn Transport,
    query: &Query,
    network: bool,
    stopped: &dyn Fn() -> bool,
) -> Result<Option<Value>> {
    if let Some((value, expires)) = cached(store, query)?
        && (expires > now() || !network)
    {
        return Ok(Some(value));
    }
    if !network || stopped() {
        return Ok(None);
    }
    let value = transport.get(query, stopped)?;
    if stopped() {
        return Ok(None);
    }
    let ttl = if value["releases"].as_array().is_some_and(Vec::is_empty) {
        WEEK
    } else {
        30 * 86400
    };
    store.0.execute("INSERT INTO metadata_http(key,value,expires) VALUES (?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,expires=excluded.expires",params![query.key(),value.to_string(),now()+ttl])?;
    Ok(Some(value))
}

/// Reuses cached responses even with networking disabled. Calls update only for changed
/// albums. A transport failure ends the network pass; local results remain available.
pub fn enrich(
    library: &mut Library,
    store: &Store,
    network: &dyn Fn() -> bool,
    cancelled: &dyn Fn() -> bool,
    update: &mut dyn FnMut(&Album),
) -> Result<()> {
    enrich_with(
        library,
        store,
        &MusicBrainz::default(),
        network,
        cancelled,
        update,
    )
}
fn enrich_with(
    library: &mut Library,
    store: &Store,
    transport: &dyn Transport,
    network: &dyn Fn() -> bool,
    cancelled: &dyn Fn() -> bool,
    update: &mut dyn FnMut(&Album),
) -> Result<()> {
    // Prioritize discs: a network library may have thousands of untagged files
    // whose probes would otherwise delay every raw-disc lookup.
    for discs in [true, false] {
        let mut work = Vec::new();
        for (index, album) in library.albums.iter_mut().enumerate() {
            let is_disc = album.tracks.first().is_some_and(|t| {
                matches!(
                    t.access,
                    MediaAccess::DiscTrack { .. } | MediaAccess::RedumperTrack { .. }
                )
            });
            if is_disc != discs {
                continue;
            }
            if cancelled() {
                return Ok(());
            }
            let query = match query_for(album) {
                Ok(Some(q)) => q,
                Ok(None) => continue,
                Err(e) => {
                    set_lookup_status(album, &format!("source read failed: {e:#}"));
                    update(album);
                    continue;
                }
            };
            resolve(album, store, transport, &query, false, cancelled, update)?;
            work.push((index, query));
        }
        for (index, query) in work {
            if cancelled() || !network() {
                break;
            }
            let stopped = || cancelled() || !network();
            let album = &mut library.albums[index];
            if let Err(e) = resolve(album, store, transport, &query, true, &stopped, update) {
                set_lookup_status(album, &format!("lookup failed: {e:#}"));
                update(album);
                return Err(e);
            }
        }
    }
    Ok(())
}
fn set_lookup_status(album: &mut Album, status: &str) -> bool {
    let before = album.source_label.clone();
    if let Some(i) = album.source_label.find(" • MusicBrainz") {
        album.source_label.truncate(i);
    }
    album
        .source_label
        .push_str(&format!(" • MusicBrainz: {status}"));
    before != album.source_label
}
fn resolve(
    album: &mut Album,
    store: &Store,
    transport: &dyn Transport,
    query: &Query,
    network: bool,
    stopped: &dyn Fn() -> bool,
    update: &mut dyn FnMut(&Album),
) -> Result<()> {
    let Some(mut value) = response(store, transport, query, network, stopped)? else {
        return Ok(());
    };
    if let Query::Search { title, artist } = query {
        let releases = value["releases"]
            .as_array()
            .context("Missing MusicBrainz search results")?;
        // No arbitrary first result or edition selection, including truncated search results.
        if value["count"].as_u64() != Some(1) || releases.len() != 1 {
            return Ok(());
        }
        let r = &releases[0];
        if normalized(&string(r, "title")) != normalized(title)
            || normalized(&credit(&r["artist-credit"])) != normalized(artist)
        {
            return Ok(());
        }
        let id = string(r, "id");
        if !mbid(&id) {
            return Ok(());
        }
        let Some(full) = response(store, transport, &Query::Release(id), network, stopped)? else {
            return Ok(());
        };
        value = full;
    }
    let record = choose(album, query, &value);
    store.0.execute("INSERT INTO metadata_matches(album_id,value) VALUES (?1,?2) ON CONFLICT(album_id) DO UPDATE SET value=excluded.value",params![serde_json::to_string(&album.id)?,serde_json::to_string(&record)?])?;
    if !stopped() && record.names.is_none() && set_lookup_status(album, &record.status) {
        update(album);
    }
    if !stopped()
        && let Some(names) = record.names
    {
        let changed = apply(album, &names);
        let status_changed = set_lookup_status(album, &record.status);
        if changed || status_changed {
            update(album);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use amp_core::{ItemId, Track};
    use std::cell::Cell;
    fn album() -> Album {
        let mut album = Album {
            metadata: Default::default(),
            id: ItemId {
                library: "test".into(),
                provider: "filesystem".into(),
                item: "disc".into(),
            },
            title: "disc".into(),
            artist: "Unknown artist".into(),
            source_label: "BIN/CUE".into(),
            tracks: (1..=2)
                .map(|number| Track {
                    blocked_reason: None,
                    availability: Default::default(),
                    source: None,
                    id: ItemId {
                        library: "test".into(),
                        provider: "file".into(),
                        item: number.to_string(),
                    },
                    title: format!("Track {number}"),
                    artist: "Unknown artist".into(),
                    number,
                    disc: 1,
                    duration_seconds: 1.0,
                    access: MediaAccess::DiscTrack {
                        cue: "/test/disc.cue".into(),
                        number: number as u8,
                    },
                })
                .collect(),
        };
        album.metadata.names.title = NameOrigin::Generated(album.title.clone());
        album.metadata.names.artist = NameOrigin::Generated(album.artist.clone());
        for track in &album.tracks {
            album.metadata.tracks.insert(
                track.id.item.clone(),
                amp_core::NameOrigins {
                    title: NameOrigin::Generated(track.title.clone()),
                    artist: NameOrigin::Generated(track.artist.clone()),
                },
            );
        }
        album
    }
    fn release(id: &str, title: &str) -> Value {
        json!({"id":id,"title":title,"artist-credit":[{"name":"The Artist","joinphrase":""}],
        "media":[{"position":1,"discs":[{"id":"test-id"}],"tracks":[
            {"position":1,"title":"First song","length":1000,"artist-credit":[{"name":"Singer One"}]},
            {"position":2,"recording":{"title":"Second song","length":1000,"artist-credit":[{"name":"Singer Two"}]}}
        ]}]})
    }
    struct Fake {
        calls: Cell<usize>,
        value: Value,
        fail: bool,
    }
    impl Transport for Fake {
        fn get(&self, _: &Query, _: &dyn Fn() -> bool) -> Result<Value> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                bail!("offline")
            }
            Ok(self.value.clone())
        }
    }
    #[test]
    fn missing_album_tag_uses_release_id_and_fills_folder_fallback() {
        use amp_core::LibraryProvider;
        use lofty::{
            config::WriteOptions,
            prelude::{Accessor, TagExt},
            tag::{Tag, TagType},
        };
        let dir = tempfile::tempdir().unwrap();
        let release_id = "12345678-1234-1234-1234-123456789abc";
        for number in 1..=2 {
            let path = dir.path().join(format!("{number}.wav"));
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
            for _ in 0..88200 {
                wav.write_sample(0i16).unwrap();
            }
            wav.finalize().unwrap();
            let mut tag = Tag::new(TagType::Id3v2);
            tag.set_title(format!("Local title {number}"));
            tag.set_artist("Local artist".into());
            tag.set_track(number);
            let mut tag = lofty::id3::v2::Id3v2Tag::from(tag);
            tag.insert(lofty::id3::v2::Frame::UserText(
                lofty::id3::v2::ExtendedTextFrame::new(
                    lofty::TextEncoding::UTF8,
                    "MusicBrainz Album Id",
                    release_id,
                ),
            ));
            tag.save_to_path(&path, WriteOptions::default()).unwrap();
        }
        let library = crate::FilesystemProvider {
            root: dir.path().into(),
            preferences: Default::default(),
        }
        .scan(&|| false)
        .unwrap();
        let mut a = library.albums[0].clone();
        assert!(matches!(a.metadata.names.title, NameOrigin::Generated(_)));
        let Some(Query::Release(id)) = query_for(&a).unwrap() else {
            panic!("release ID was skipped")
        };
        assert_eq!(id, release_id);
        let names = candidate(
            &a,
            &release(release_id, "The actual album"),
            &Query::Release(id),
        )
        .unwrap();
        assert!(apply(&mut a, &names));
        assert_eq!(a.title, "The actual album");
        assert_eq!(a.metadata.names.title, NameOrigin::MusicBrainz);
        assert_eq!(a.tracks[0].title, "Local title 1");
        assert_eq!(a.artist, "Local artist");
        let restored: Album = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(restored.metadata.names.title, NameOrigin::MusicBrainz);
    }
    #[test]
    fn provenance_preserves_literal_tags_corrections_and_uncertain_legacy_names() {
        let names = candidate(
            &album(),
            &release("one", "Online album"),
            &Query::Disc("test-id".into()),
        )
        .unwrap();
        let mut tagged = album();
        tagged.metadata.names.title = NameOrigin::Source;
        tagged
            .metadata
            .tracks
            .get_mut(&tagged.tracks[0].id.item)
            .unwrap()
            .title = NameOrigin::Source;
        apply(&mut tagged, &names);
        assert_eq!(tagged.title, "disc");
        assert_eq!(tagged.tracks[0].title, "Track 1");
        let mut corrected = album();
        corrected.title = "A local correction".into();
        corrected.tracks[0].title = "Corrected song".into();
        apply(&mut corrected, &names);
        assert_eq!(corrected.title, "A local correction");
        assert_eq!(corrected.tracks[0].title, "Corrected song");
        let mut legacy = serde_json::to_value(album()).unwrap();
        legacy.as_object_mut().unwrap().remove("metadata");
        let mut legacy: Album = serde_json::from_value(legacy).unwrap();
        apply(&mut legacy, &names);
        assert_eq!(legacy.title, "disc");
        assert_eq!(legacy.tracks[0].title, "Track 1");
        assert_eq!(legacy.artist, "The Artist");
    }
    #[test]
    fn musicbrainz_documented_disc_id_vector() {
        let offsets = [
            150, 22767, 41887, 58317, 72102, 91375, 104652, 115380, 132165, 143932, 159870, 174597,
        ];
        assert_eq!(
            disc_id(&offsets, 267257).unwrap(),
            "I5l9cCSFccLKFEKS.7wqSZAorPU-"
        );
        assert!(disc_id(&[150, 149], 200).is_err());
        assert!(disc_id(&[], 200).is_err());
    }
    #[test]
    fn edition_consensus_fills_missing_names_and_preserves_local_values_and_access() {
        let mut a = album();
        a.tracks[0].title = "My local correction".into();
        let before = serde_json::to_string(&a.tracks[0].access).unwrap();
        let q = Query::Disc("test-id".into());
        let r = choose(
            &a,
            &q,
            &json!({"releases":[release("one","An Album"),release("two","An Album")]}),
        );
        assert_eq!(r.status, "matched");
        assert_eq!(r.releases.len(), 2);
        assert!(apply(&mut a, &r.names.unwrap()));
        assert_eq!(a.title, "An Album");
        assert_eq!(a.artist, "The Artist");
        assert_eq!(a.tracks[0].title, "My local correction");
        assert_eq!(a.tracks[1].title, "Second song");
        assert_eq!(a.tracks[1].artist, "Singer Two");
        assert_eq!(serde_json::to_string(&a.tracks[0].access).unwrap(), before);
        let r = choose(
            &a,
            &q,
            &json!({"releases":[release("one","An Album"),release("two","Other Album")]}),
        );
        assert_eq!(r.status, "ambiguous");
        assert!(r.names.is_none());
    }
    #[test]
    fn translated_track_lists_preserve_agreed_album_identity() {
        let mut translated = release("two", "Album");
        translated["media"][0]["tracks"][0]["title"] = json!("Translated song");
        let q = Query::Disc("test-id".into());
        let record = choose(
            &album(),
            &q,
            &json!({"releases": [release("one", "Album"), translated]}),
        );
        assert_eq!(record.status, "ambiguous");
        let mut a = album();
        apply(&mut a, &record.names.unwrap());
        assert_eq!(a.title, "Album");
        assert_eq!(a.artist, "The Artist");
        assert_eq!(a.tracks[0].title, "Track 1");
        assert_eq!(a.tracks[1].title, "Second song");
    }
    #[test]
    fn matching_rejects_wrong_medium_track_counts_and_duplicate_slots() {
        let mut a = album();
        let q = Query::Disc("other-id".into());
        assert!(candidate(&a, &release("one", "Album"), &q).is_none());
        a.tracks[1].number = 1;
        assert!(candidate(&a, &release("one", "Album"), &Query::Disc("test-id".into())).is_none());
        let mut a = album();
        a.tracks.pop();
        assert!(candidate(&a, &release("one", "Album"), &Query::Disc("test-id".into())).is_none());
    }
    #[test]
    fn name_search_requires_duration_and_existing_track_title_agreement() {
        let mut a = album();
        let q = Query::Search {
            title: "Album".into(),
            artist: "The Artist".into(),
        };
        assert!(candidate(&a, &release("one", "Album"), &q).is_some());
        a.tracks[0].duration_seconds = 10.0;
        assert!(candidate(&a, &release("one", "Album"), &q).is_none());
        a.tracks[0].duration_seconds = 1.0;
        a.tracks[0].title = "Different track".into();
        assert!(candidate(&a, &release("one", "Album"), &q).is_none());
    }
    #[test]
    fn positive_negative_and_stale_cache_work_offline_without_requests() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.db")).unwrap();
        let fake = Fake {
            calls: Cell::new(0),
            value: json!({"releases":[release("one","Album")]}),
            fail: false,
        };
        let q = Query::Disc("test-id".into());
        let mut a = album();
        resolve(&mut a, &store, &fake, &q, false, &|| false, &mut |_| {}).unwrap();
        assert_eq!(fake.calls.get(), 0);
        resolve(&mut a, &store, &fake, &q, true, &|| false, &mut |_| {}).unwrap();
        assert_eq!(fake.calls.get(), 1);
        assert_eq!(a.title, "Album");
        store
            .0
            .execute("UPDATE metadata_http SET expires=0", [])
            .unwrap();
        let mut another = album();
        resolve(
            &mut another,
            &store,
            &fake,
            &q,
            false,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(another.title, "Album");
        assert_eq!(fake.calls.get(), 1);
        let fake = Fake {
            calls: Cell::new(0),
            value: json!({"releases":[]}),
            fail: false,
        };
        let q = Query::Disc("unknown-id".into());
        response(&store, &fake, &q, true, &|| false).unwrap();
        response(&store, &fake, &q, true, &|| false).unwrap();
        assert_eq!(fake.calls.get(), 1);
        let q = Query::Disc("cancelled".into());
        response(&store, &fake, &q, true, &|| true).unwrap();
        assert_eq!(fake.calls.get(), 1);
    }
    #[test]
    fn failed_network_does_not_overwrite_cache_or_local_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.db")).unwrap();
        let q = Query::Disc("test-id".into());
        let value = json!({"releases":[release("one","Cached Album")]});
        store
            .0
            .execute(
                "INSERT INTO metadata_http VALUES (?1,?2,0)",
                params![q.key(), value.to_string()],
            )
            .unwrap();
        let fake = Fake {
            calls: Cell::new(0),
            value: Value::Null,
            fail: true,
        };
        let mut a = album();
        resolve(&mut a, &store, &fake, &q, false, &|| false, &mut |_| {}).unwrap();
        assert!(resolve(&mut a, &store, &fake, &q, true, &|| false, &mut |_| {}).is_err());
        assert_eq!(a.title, "Cached Album");
        assert_eq!(cached(&store, &q).unwrap().unwrap().0, value);
    }
    #[test]
    #[ignore = "live MusicBrainz API smoke test; requires network"]
    fn live_documented_disc_lookup_and_names() {
        let q = Query::Disc("I5l9cCSFccLKFEKS.7wqSZAorPU-".into());
        let value = MusicBrainz::default().get(&q, &|| false).unwrap();
        let mut a = album();
        let template = a.tracks[0].clone();
        a.tracks = (1..=12)
            .map(|number| {
                let mut t = template.clone();
                t.number = number;
                t.title = format!("Track {number}");
                t
            })
            .collect();
        let record = choose(&a, &q, &value);
        assert_eq!(record.status, "matched");
        apply(&mut a, &record.names.unwrap());
        assert_eq!(a.title, "Nevermind");
        assert_eq!(a.artist, "Nirvana");
        assert_eq!(a.tracks.len(), 12);
        assert_eq!(a.tracks[0].title, "Smells Like Teen Spirit");
    }
    #[test]
    fn cue_toc_accounts_for_synthetic_initial_pregap_and_excludes_mixed_mode() {
        let dir = tempfile::tempdir().unwrap();
        let cue = dir.path().join("disc.cue");
        std::fs::write(dir.path().join("disc.bin"), vec![0; 2352 * 4]).unwrap();
        let mut a = album();
        for t in &mut a.tracks {
            t.access = MediaAccess::DiscTrack {
                cue: cue.to_string_lossy().into(),
                number: t.number as u8,
            };
        }
        for pre in ["", "PREGAP 00:02:00"] {
            std::fs::write(&cue,format!("FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n {pre}\n INDEX 01 00:00:00\n TRACK 02 AUDIO\n INDEX 01 00:00:02\n")).unwrap();
            let Some(Query::Disc(id)) = query_for(&a).unwrap() else {
                panic!("missing disc ID")
            };
            assert_eq!(id, disc_id(&[150, 152], 154).unwrap());
        }
        std::fs::write(&cue,"FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n INDEX 01 00:00:00\n TRACK 02 MODE1/2352\n INDEX 01 00:00:02\n").unwrap();
        assert!(query_for(&a).unwrap().is_none());
    }
    #[test]
    fn cancellation_during_request_discards_the_response() {
        struct Cancels<'a>(&'a Cell<bool>);
        impl Transport for Cancels<'_> {
            fn get(&self, _: &Query, _: &dyn Fn() -> bool) -> Result<Value> {
                self.0.set(true);
                Ok(json!({"releases":[]}))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.db")).unwrap();
        let stop = Cell::new(false);
        let q = Query::Disc("cancelled".into());
        assert!(
            response(&store, &Cancels(&stop), &q, true, &|| stop.get())
                .unwrap()
                .is_none()
        );
        assert!(cached(&store, &q).unwrap().is_none());
    }
    #[test]
    fn background_enrichment_reuses_cache_with_network_disabled_and_never_changes_media() {
        let dir = tempfile::tempdir().unwrap();
        let cue = dir.path().join("disc.cue");
        let bin = dir.path().join("disc.bin");
        let cue_text = "FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n INDEX 01 00:00:00\n TRACK 02 AUDIO\n INDEX 01 00:00:01\n";
        let bytes = vec![5; 2352 * 2];
        std::fs::write(&cue, cue_text).unwrap();
        std::fs::write(&bin, &bytes).unwrap();
        let mut a = album();
        for t in &mut a.tracks {
            t.access = MediaAccess::DiscTrack {
                cue: cue.to_string_lossy().into(),
                number: t.number as u8,
            };
        }
        let q = query_for(&a).unwrap().unwrap();
        let Query::Disc(id) = q else {
            panic!("expected disc")
        };
        let mut r = release("one", "Cached album");
        r["media"][0]["discs"][0]["id"] = json!(id);
        let fake = Fake {
            calls: Cell::new(0),
            value: json!({"releases":[r]}),
            fail: false,
        };
        let store = Store::open(&dir.path().join("cache.db")).unwrap();
        let original = Library {
            albums: vec![a],
            ..Library::default()
        };
        let mut library = original.clone();
        enrich_with(
            &mut library,
            &store,
            &fake,
            &|| true,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fake.calls.get(), 1);
        assert_eq!(library.albums[0].title, "Cached album");
        let mut library = original;
        enrich_with(
            &mut library,
            &store,
            &fake,
            &|| false,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fake.calls.get(), 1);
        assert_eq!(library.albums[0].tracks[1].title, "Second song");
        assert_eq!(std::fs::read_to_string(cue).unwrap(), cue_text);
        assert_eq!(std::fs::read(bin).unwrap(), bytes);
    }
}
