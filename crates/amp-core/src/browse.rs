//! Provider-independent catalog search. Display credits are never split into guessed artists.
use crate::{ItemId, Library};
use std::collections::BTreeMap;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

pub fn normalized(text: &str) -> String {
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn artist_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
/// Match complete names in collaboration credits, never partial words.
pub fn credit_matches(credit: &str, name: &str) -> bool {
    let credit = normalized(credit);
    let name = normalized(name);
    !name.is_empty()
        && credit.match_indices(&name).any(|(i, _)| {
            credit[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
                && credit[i + name.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric())
        })
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Album(ItemId),
    Artist(String),
    Track { album: ItemId, track: ItemId },
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub target: Target,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub seconds: f64,
    pub album_count: usize,
    pub playback_block: Option<&'static str>,
    pub scan_status: Option<String>,
    fields: Vec<String>,
}
impl Entry {
    fn matches(&self, words: &[&str]) -> bool {
        words
            .iter()
            .all(|word| self.fields.iter().any(|field| field.contains(word)))
    }
}
#[derive(Clone, Debug, Default)]
pub struct Results {
    pub artists: Vec<Entry>,
    pub albums: Vec<Entry>,
    pub tracks: Vec<Entry>,
}
#[derive(Clone, Debug, Default)]
pub struct ArtistPage {
    pub albums: Vec<Entry>,
    pub appearances: Vec<(Entry, Vec<Entry>)>,
}
#[derive(Default)]
pub struct CatalogIndex {
    pub artists: Vec<Entry>,
    pub albums: Vec<Entry>,
    tracks: Vec<Entry>,
    album_tracks: Vec<Vec<Entry>>,
}
fn id_key(id: &ItemId) -> (&str, &str, &str) {
    (&id.library, &id.provider, &id.item)
}
impl CatalogIndex {
    pub fn new(library: &Library) -> Self {
        let mut artists = BTreeMap::<String, Entry>::new();
        let mut albums: Vec<_> = library.albums.iter().collect();
        albums.sort_by(|a, b| {
            (normalized(&a.title), normalized(&a.artist), id_key(&a.id)).cmp(&(
                normalized(&b.title),
                normalized(&b.artist),
                id_key(&b.id),
            ))
        });
        let mut index = Self::default();
        let mut sorted_tracks = Vec::new();
        for album in albums {
            let entry = Entry {
                target: Target::Album(album.id.clone()),
                title: album.title.clone(),
                artist: album.artist.clone(),
                album: album.title.clone(),
                seconds: 0.0,
                album_count: 0,
                playback_block: None,
                scan_status: None,
                fields: vec![normalized(&album.title), normalized(&album.artist)],
            };
            index.albums.push(entry);
            let mut add_artist = |name: &str, album_credit: bool| {
                if name.trim().is_empty() {
                    return;
                }
                let a = artists.entry(artist_key(name)).or_insert_with(|| Entry {
                    target: Target::Artist(name.to_owned()),
                    title: name.to_owned(),
                    artist: name.to_owned(),
                    album: String::new(),
                    seconds: 0.0,
                    album_count: 0,
                    playback_block: None,
                    scan_status: None,
                    fields: vec![normalized(name)],
                });
                a.album_count += usize::from(album_credit);
            };
            add_artist(&album.artist, true);
            let mut tracks = Vec::new();
            for track in &album.tracks {
                add_artist(&track.artist, false);
                let entry = Entry {
                    target: Target::Track {
                        album: album.id.clone(),
                        track: track.id.clone(),
                    },
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    album: album.title.clone(),
                    seconds: track.duration_seconds,
                    album_count: 0,
                    playback_block: track.playback_block(),
                    scan_status: track
                        .scan_status
                        .as_ref()
                        .map(|status| status.label().to_owned()),
                    fields: vec![
                        normalized(&track.title),
                        normalized(&track.artist),
                        normalized(&album.title),
                        normalized(&album.artist),
                    ],
                };
                sorted_tracks.push((
                    (
                        normalized(&track.title),
                        normalized(&track.artist),
                        normalized(&album.title),
                        track.disc,
                        track.number,
                        (
                            track.id.library.clone(),
                            track.id.provider.clone(),
                            track.id.item.clone(),
                        ),
                    ),
                    entry.clone(),
                ));
                tracks.push(entry);
            }
            index.album_tracks.push(tracks);
        }
        sorted_tracks.sort_by(|a, b| a.0.cmp(&b.0));
        index.tracks = sorted_tracks.into_iter().map(|(_, entry)| entry).collect();
        index.artists = artists.into_values().collect();
        index
            .artists
            .sort_by_key(|a| (normalized(&a.title), a.title.clone()));
        index
    }
    pub fn search(&self, query: &str) -> Results {
        let query = normalized(query);
        let words: Vec<_> = query.split_whitespace().collect();
        if words.is_empty() {
            return Results::default();
        }
        let matching = |entries: &[Entry]| {
            entries
                .iter()
                .filter(|e| e.matches(&words))
                .cloned()
                .collect()
        };
        Results {
            artists: matching(&self.artists),
            albums: matching(&self.albums),
            tracks: matching(&self.tracks),
        }
    }
    pub fn album_artists(&self) -> Vec<Entry> {
        self.artists
            .iter()
            .filter(|a| a.album_count > 0)
            .cloned()
            .collect()
    }
    pub fn artist(&self, name: &str) -> ArtistPage {
        let mut page = ArtistPage::default();
        for (album, tracks) in self.albums.iter().zip(&self.album_tracks) {
            if credit_matches(&album.artist, name) {
                page.albums.push(album.clone());
            } else {
                let matching: Vec<_> = tracks
                    .iter()
                    .filter(|t| credit_matches(&t.artist, name))
                    .cloned()
                    .collect();
                if !matching.is_empty() {
                    page.appearances.push((album.clone(), matching));
                }
            }
        }
        page
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Album, MediaAccess, Track};
    fn id(s: &str) -> ItemId {
        ItemId {
            library: "library".into(),
            provider: "managed".into(),
            item: s.into(),
        }
    }
    fn album(key: &str, title: &str, artist: &str, tracks: &[(&str, &str)]) -> Album {
        Album {
            metadata: Default::default(),
            id: id(key),
            title: title.into(),
            artist: artist.into(),
            source_label: String::new(),
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (title, artist))| Track {
                    scan_status: None,
                    blocked_reason: None,
                    availability: Default::default(),
                    source: None,
                    id: id(&format!("{key}/{i}")),
                    title: (*title).into(),
                    artist: (*artist).into(),
                    number: i as u32 + 1,
                    disc: 1,
                    duration_seconds: 60.0,
                    access: MediaAccess::Managed {
                        provider: "managed".into(),
                        item: format!("{key}/{i}"),
                    },
                })
                .collect(),
        }
    }
    fn fixture() -> Library {
        Library {
            albums: vec![
                album(
                    "heroes",
                    "Heroes",
                    "David Bowie",
                    &[("Beauty and the Beast", "David Bowie")],
                ),
                album(
                    "joint",
                    "Under Pressure",
                    "Queen & David Bowie",
                    &[("Under Pressure", "Queen & David Bowie")],
                ),
                album(
                    "comp",
                    "Guests",
                    "Various artists",
                    &[
                        ("Guest song", "David Bowie feat. Guest"),
                        ("Other song", "Guest"),
                    ],
                ),
                album("debut", "Début", "Björk", &[("Human Behaviour", "Björk")]),
                album("copy", "Début", "Björk", &[("Human Behaviour", "Björk")]),
            ],
            ..Default::default()
        }
    }
    #[test]
    fn search_crosses_fields_without_leaking_track_fields_into_albums() {
        let index = CatalogIndex::new(&fixture());
        let hits = index.search("beast BOWIE her");
        assert_eq!(hits.tracks.len(), 1);
        assert!(hits.albums.is_empty());
        assert!(hits.artists.is_empty());
        assert_eq!(index.search("debut BJORK").tracks.len(), 2);
        assert_eq!(index.search("BJORK").albums.len(), 2);
        assert!(index.search("   ").tracks.is_empty());
        assert!(index.search("bowie missing").tracks.is_empty());
        assert_eq!(index.search("guest").artists.len(), 2);
    }
    #[test]
    fn artist_browser_uses_complete_album_credits_and_retains_copies() {
        let index = CatalogIndex::new(&fixture());
        let names: Vec<_> = index
            .album_artists()
            .into_iter()
            .map(|a| (a.title, a.album_count))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Björk".into(), 2),
                ("David Bowie".into(), 1),
                ("Queen & David Bowie".into(), 1),
                ("Various artists".into(), 1)
            ]
        );
        let page = index.artist("David Bowie");
        assert_eq!(page.albums.len(), 2);
        assert_eq!(page.appearances.len(), 1);
        assert_eq!(page.appearances[0].1.len(), 1);
        assert_eq!(index.artist("Guest").appearances[0].1.len(), 2);
        assert_eq!(index.artist("Björk").albums.len(), 2);
    }
    #[test]
    fn matching_respects_names_and_keeps_accented_identities_distinct() {
        assert!(!credit_matches("Joanne", "Ann"));
        assert!(!credit_matches("Anna", "Ann"));
        assert!(credit_matches("Artist feat. Ann", "ann"));
        assert!(credit_matches("Björk & Friends", "Bjork"));
        assert!(!credit_matches("Anyone", ""));
        let library = Library {
            albums: vec![
                album("a", "A", " Björk ", &[]),
                album("b", "B", "BJÖRK", &[]),
                album("c", "C", "Bjork", &[]),
            ],
            ..Default::default()
        };
        let index = CatalogIndex::new(&library);
        assert_eq!(index.artists.len(), 2);
        assert_eq!(index.search("bjork").artists.len(), 2);
    }
    #[test]
    fn ten_thousand_albums_search_benchmark() {
        let library = Library {
            albums: (0..10_000)
                .map(|i| {
                    album(
                        &i.to_string(),
                        &format!("Album {i:05}"),
                        &format!("Artist {}", i % 1000),
                        &[("First", "Guest"); 10],
                    )
                })
                .collect(),
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let index = CatalogIndex::new(&library);
        let built = start.elapsed();
        let start = std::time::Instant::now();
        let found = index.search("artist 123 first");
        let searched = start.elapsed();
        assert_eq!(index.tracks.len(), 100_000);
        assert!(!found.tracks.is_empty());
        eprintln!(
            "10,000 albums / 100,000 tracks: index {built:?}, search {searched:?}, {} matches",
            found.tracks.len()
        );
    }
}
