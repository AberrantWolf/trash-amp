use super::*;
use crate::coverage::DirectoryCoverage;
use amp_core::{Availability, NameOrigin, NameOrigins, PlaybackBlockReason};
use std::collections::BTreeMap;

const ROOT: &str = "/music";

fn track(relative: &str, title: &str, number: u32) -> Track {
    let locator = format!("{ROOT}/{relative}");
    Track {
        scan_status: None,
        blocked_reason: None,
        availability: Availability::Available,
        source: None,
        id: ItemId {
            library: ROOT.into(),
            provider: "filesystem".into(),
            item: locator.clone(),
        },
        title: title.into(),
        artist: "An artist".into(),
        number,
        disc: 1,
        duration_seconds: 123.0,
        access: MediaAccess::File { locator },
    }
}

fn album(id: &str, tracks: Vec<Track>) -> Album {
    let mut album = Album {
        metadata: Default::default(),
        id: ItemId {
            library: ROOT.into(),
            provider: "filesystem".into(),
            item: id.into(),
        },
        title: "A tagged album".into(),
        artist: "An artist".into(),
        tracks,
        source_label: ROOT.into(),
    };
    album.metadata.names = NameOrigins {
        title: NameOrigin::Source,
        artist: NameOrigin::Source,
    };
    for track in &album.tracks {
        album.metadata.tracks.insert(
            track.id.item.clone(),
            NameOrigins {
                title: NameOrigin::Source,
                artist: NameOrigin::Source,
            },
        );
    }
    album
}

fn library(albums: Vec<Album>) -> Library {
    Library {
        scan: Some(ScanSummary {
            source_identity: Some("nas://music".into()),
            ..Default::default()
        }),
        id: ROOT.into(),
        root: ROOT.into(),
        albums,
        warnings: vec![],
    }
}

fn coverage() -> Coverage {
    Coverage {
        mounts: Default::default(),
        root: ROOT.into(),
        directories: BTreeMap::new(),
        files: BTreeSet::new(),
        failures: BTreeMap::new(),
        issues: vec![],
        source_disconnected: false,
        source_identity: Some("nas://music".into()),
    }
}

fn directory(coverage: &mut Coverage, relative: &str, complete: bool, children: &[&str]) {
    let path = Path::new(ROOT).join(relative);
    coverage.directories.insert(
        path.clone(),
        DirectoryCoverage {
            complete,
            children: children.iter().map(|child| path.join(child)).collect(),
        },
    );
}

fn reconcile(old: &Library, albums: Vec<Album>, coverage: Coverage) -> Library {
    ScanResult {
        library: library(albums),
        coverage: Some(coverage),
    }
    .reconcile(Some(old))
}

fn tracks(library: &Library) -> Vec<&Track> {
    library
        .albums
        .iter()
        .flat_map(|album| &album.tracks)
        .collect()
}

#[test]
fn a_complete_parent_listing_removes_a_deleted_file_and_adds_a_new_one() {
    let kept = track("album/one.wav", "One", 1);
    let removed = track("album/two.wav", "Two", 2);
    let added = track("album/three.wav", "Three", 3);
    let old = library(vec![album("album", vec![kept.clone(), removed])]);
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["album"]);
    directory(&mut evidence, "album", true, &["one.wav", "three.wav"]);
    let updated = reconcile(&old, vec![album("album", vec![kept, added])], evidence);
    assert_eq!(
        tracks(&updated)
            .iter()
            .map(|track| track.title.as_str())
            .collect::<Vec<_>>(),
        ["One", "Three"]
    );
    let summary = updated.scan.unwrap();
    assert_eq!(
        (summary.added, summary.removed, summary.unverified),
        (1, 1, 0)
    );
    assert!(!summary.incomplete);
}

#[test]
fn a_deleted_directory_removes_its_cached_descendants() {
    let old = library(vec![album(
        "deleted",
        vec![
            track("deleted/disc1/one.wav", "One", 1),
            track("deleted/disc2/two.wav", "Two", 2),
        ],
    )]);
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &[]);
    let updated = reconcile(&old, vec![], evidence);
    assert!(updated.albums.is_empty());
    assert_eq!(updated.scan.unwrap().removed, 2);
}

#[test]
fn an_unreadable_subtree_keeps_old_entries_without_changing_playback_eligibility() {
    let playable = track("private/one.wav", "One", 1);
    let mut blocked = track("private/two.wav", "Two", 2);
    blocked.availability = Availability::Unavailable;
    blocked.blocked_reason = Some(PlaybackBlockReason::Missing);
    let old = library(vec![album("private", vec![playable, blocked])]);
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["private"]);
    directory(&mut evidence, "private", false, &[]);
    evidence.issues.push(ScanIssue {
        path: format!("{ROOT}/private"),
        reason: "Permission denied".into(),
    });
    let updated = reconcile(&old, vec![], evidence);
    let entries = tracks(&updated);
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|track| matches!(&track.scan_status,
        Some(ScanStatus::Unverified { path, reason })
            if path == "/music/private" && reason == "Permission denied")));
    assert_eq!(entries[0].playback_block(), None);
    assert_eq!(entries[0].availability, Availability::Available);
    assert_eq!(
        entries[1].playback_block(),
        Some("Music item no longer available")
    );
    assert_eq!(entries[1].availability, Availability::Unavailable);
    assert_eq!(updated.scan.as_ref().unwrap().removed, 0);
    let restored: Library =
        serde_json::from_str(&serde_json::to_string(&updated).unwrap()).unwrap();
    assert_eq!(tracks(&restored)[0].scan_status, entries[0].scan_status);
    assert_eq!(restored.scan.unwrap().unverified, 2);
}

#[test]
fn source_loss_invalidates_earlier_deletion_evidence_but_accepts_readable_additions() {
    let old = library(vec![album(
        "album",
        vec![
            track("album/deleted.wav", "Deleted earlier", 1),
            track("later/two.wav", "Not checked", 2),
        ],
    )]);
    let new = track("album/new.wav", "New music", 3);
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["album", "later"]);
    directory(&mut evidence, "album", true, &["new.wav"]);
    directory(&mut evidence, "later", false, &[]);
    assert!(evidence.removal_proven(Path::new("/music/album/deleted.wav")));
    evidence.source_disconnected = true;
    evidence.issues.push(ScanIssue {
        path: "/music/later".into(),
        reason: "Network connection lost".into(),
    });
    let updated = reconcile(&old, vec![album("album", vec![new])], evidence);
    let entries = tracks(&updated);
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries
            .iter()
            .filter(|track| track.scan_status.is_some())
            .count(),
        2
    );
    assert!(
        entries
            .iter()
            .find(|track| track.title == "New music")
            .unwrap()
            .scan_status
            .is_none()
    );
    let summary = updated.scan.unwrap();
    assert!(summary.source_disconnected);
    assert_eq!(
        (summary.added, summary.removed, summary.unverified),
        (1, 0, 2)
    );
    assert!(summary.message().contains("no tracks removed"));
}

#[test]
fn a_failed_folder_does_not_block_confirmed_deletions_in_another_folder() {
    let old = library(vec![
        album("checked", vec![track("checked/deleted.wav", "Deleted", 1)]),
        album("private", vec![track("private/one.wav", "Not checked", 1)]),
    ]);
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["checked", "private"]);
    directory(&mut evidence, "checked", true, &[]);
    directory(&mut evidence, "private", false, &[]);
    evidence.issues.push(ScanIssue {
        path: "/music/private".into(),
        reason: "Permission denied".into(),
    });
    let updated = reconcile(&old, vec![], evidence);
    assert_eq!(tracks(&updated).len(), 1);
    assert_eq!(tracks(&updated)[0].title, "Not checked");
    let summary = updated.scan.unwrap();
    assert_eq!((summary.removed, summary.unverified), (1, 1));
    assert!(summary.incomplete);
    assert!(!summary.source_disconnected);
}

#[test]
fn losing_a_nested_mount_suppresses_deletions_even_when_the_root_is_healthy() {
    let mut old = library(vec![album(
        "share",
        vec![track("share/song.wav", "Offline song", 1)],
    )]);
    old.scan
        .as_mut()
        .unwrap()
        .mounts
        .insert("/music/share".into(), "nas://nested".into());
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["share"]);
    directory(&mut evidence, "share", true, &[]);
    assert!(evidence.removal_proven(Path::new("/music/share/song.wav")));
    let updated = reconcile(&old, vec![], evidence);
    assert_eq!(tracks(&updated).len(), 1);
    assert_eq!(updated.scan.as_ref().unwrap().removed, 0);
    assert!(updated.scan.as_ref().unwrap().source_disconnected);
    assert_eq!(
        updated.scan.as_ref().unwrap().mounts["/music/share"],
        "nas://nested"
    );
    let mut again = coverage();
    directory(&mut again, "", true, &["share"]);
    directory(&mut again, "share", true, &[]);
    let updated = reconcile(&updated, vec![], again);
    assert_eq!(tracks(&updated).len(), 1);
    assert!(updated.scan.unwrap().source_disconnected);
}

#[test]
fn a_replacement_nested_volume_cannot_overwrite_names_or_add_unrelated_tracks() {
    let original = track("share/song.wav", "Original song", 1);
    let mut old = library(vec![album("share", vec![original])]);
    old.scan
        .as_mut()
        .unwrap()
        .mounts
        .insert("/music/share".into(), "original-volume".into());
    let mut evidence = coverage();
    evidence
        .mounts
        .insert("/music/share".into(), "replacement-volume".into());
    evidence
        .mounts
        .insert("/music/share/nested".into(), "replacement-child".into());
    directory(&mut evidence, "", true, &["share", "good"]);
    directory(&mut evidence, "share", true, &["song.wav", "extra.wav"]);
    let mut foreign = album(
        "share",
        vec![
            track("share/song.wav", "Wrong song", 1),
            track("share/extra.wav", "Unrelated", 2),
        ],
    );
    foreign.title = "Wrong album".into();
    let good = album("good", vec![track("good/new.wav", "Readable new song", 1)]);
    let updated = reconcile(&old, vec![foreign, good], evidence);
    assert_eq!(tracks(&updated).len(), 2);
    let saved = updated
        .albums
        .iter()
        .find(|album| album.id.item == "share")
        .unwrap();
    assert_eq!(saved.title, "A tagged album");
    assert_eq!(saved.tracks[0].title, "Original song");
    assert!(saved.tracks[0].scan_status.is_some());
    assert!(saved.tracks[0].playback_block().is_none());
    let summary = updated.scan.unwrap();
    assert_eq!((summary.added, summary.removed), (1, 0));
    assert_eq!(summary.mounts["/music/share"], "original-volume");
    assert!(!summary.mounts.contains_key("/music/share/nested"));
}

#[test]
fn failed_metadata_with_the_same_album_id_keeps_album_names_too() {
    let mut old = library(vec![album(
        "album",
        vec![track("album/one.wav", "Known song", 1)],
    )]);
    old.albums[0].title = "Known album".into();
    let mut fallback = album("album", vec![track("album/one.wav", "one", 1)]);
    fallback.title = "folder".into();
    fallback.tracks[0].scan_status = Some(ScanStatus::MetadataUnreadable {
        reason: "bad tags".into(),
    });
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["album"]);
    directory(&mut evidence, "album", true, &["one.wav"]);
    evidence
        .failures
        .insert("/music/album/one.wav".into(), "bad tags".into());
    let updated = reconcile(&old, vec![fallback], evidence);
    assert_eq!(updated.albums.len(), 1);
    assert_eq!(updated.albums[0].title, "Known album");
    assert_eq!(updated.albums[0].tracks[0].title, "Known song");
}

#[test]
fn a_partial_album_does_not_replace_online_names_with_folder_fallbacks() {
    let mut old = library(vec![album(
        "album",
        vec![
            track("album/one.wav", "One", 1),
            track("album/two.wav", "Two", 2),
        ],
    )]);
    old.albums[0].title = "Online title".into();
    old.albums[0].artist = "Online artist".into();
    old.albums[0].metadata.names = NameOrigins {
        title: NameOrigin::MusicBrainz,
        artist: NameOrigin::MusicBrainz,
    };
    let mut fresh = album("album", vec![track("album/one.wav", "One", 1)]);
    fresh.title = "folder".into();
    fresh.artist = "Unknown artist".into();
    fresh.metadata.names = NameOrigins {
        title: NameOrigin::Generated(fresh.title.clone()),
        artist: NameOrigin::Generated(fresh.artist.clone()),
    };
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["album"]);
    directory(&mut evidence, "album", true, &["one.wav", "two.wav"]);
    evidence
        .failures
        .insert("/music/album/two.wav".into(), "bad tags".into());
    let updated = reconcile(&old, vec![fresh], evidence);
    assert_eq!(updated.albums[0].title, "Online title");
    assert_eq!(updated.albums[0].artist, "Online artist");
    assert_eq!(
        updated.albums[0].metadata.names.title,
        NameOrigin::MusicBrainz
    );
}

#[test]
fn failed_metadata_read_uses_last_known_names_and_origins_instead_of_a_filename_fallback() {
    let original = track("album/01.wav", "A useful title", 1);
    let old = library(vec![album("tagged-album", vec![original.clone()])]);
    let mut fallback = original.clone();
    fallback.title = "01".into();
    fallback.artist = "Unknown artist".into();
    fallback.scan_status = Some(ScanStatus::MetadataUnreadable {
        reason: "Tag read failed".into(),
    });
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["album"]);
    directory(&mut evidence, "album", true, &["01.wav"]);
    evidence
        .failures
        .insert("/music/album/01.wav".into(), "Tag read failed".into());
    evidence.issues.push(ScanIssue {
        path: "/music/album/01.wav".into(),
        reason: "Tag read failed".into(),
    });
    let updated = reconcile(
        &old,
        vec![album("filename-fallback", vec![fallback])],
        evidence,
    );
    assert_eq!(updated.albums.len(), 1);
    assert_eq!(updated.albums[0].id.item, "tagged-album");
    assert_eq!(updated.albums[0].title, "A tagged album");
    let entry = tracks(&updated)[0];
    assert_eq!(entry.title, "A useful title");
    assert_eq!(entry.artist, "An artist");
    assert_eq!(entry.duration_seconds, 123.0);
    assert_eq!(entry.playback_block(), None);
    assert!(
        matches!(&entry.scan_status, Some(ScanStatus::MetadataUnreadable { reason }) if reason == "Tag read failed")
    );
    assert_eq!(
        updated.albums[0].metadata.tracks[&entry.id.item].title,
        NameOrigin::Source
    );
}

#[test]
fn a_successful_refresh_clears_old_scan_uncertainty() {
    let verified = track("album/one.wav", "One", 1);
    let mut unchecked = verified.clone();
    unchecked.scan_status = Some(ScanStatus::Unverified {
        path: "/music/album".into(),
        reason: "Permission denied".into(),
    });
    let mut old = library(vec![album("album", vec![unchecked])]);
    old.scan.as_mut().unwrap().incomplete = true;
    old.scan.as_mut().unwrap().unverified = 1;
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["album"]);
    directory(&mut evidence, "album", true, &["one.wav"]);
    let updated = reconcile(&old, vec![album("album", vec![verified])], evidence);
    assert!(tracks(&updated)[0].scan_status.is_none());
    assert_eq!(tracks(&updated)[0].playback_block(), None);
    assert!(!updated.scan.as_ref().unwrap().incomplete);
    assert_eq!(updated.scan.unwrap().unverified, 0);
}

#[test]
fn an_unmounted_source_never_becomes_the_next_refresh_baseline() {
    let old = library(vec![album("album", vec![track("one.wav", "One", 1)])]);
    let mut previous = old;
    for _ in 0..2 {
        let mut evidence = coverage();
        evidence.source_identity = Some("local-volume".into());
        directory(&mut evidence, "", true, &[]);
        previous = reconcile(&previous, vec![], evidence);
        assert_eq!(tracks(&previous).len(), 1);
        let summary = previous.scan.as_ref().unwrap();
        assert_eq!(summary.source_identity.as_deref(), Some("nas://music"));
        assert_eq!(summary.removed, 0);
        assert!(summary.source_disconnected);
    }
}

#[test]
fn partially_checked_merged_albums_keep_one_copy_of_each_track() {
    let one = track("Artist A/Collection/one.wav", "One", 1);
    let two = track("Artist B/Collection/two.wav", "Two", 2);
    let old = library(vec![album("merged-old-id", vec![one.clone(), two.clone()])]);
    let mut evidence = coverage();
    directory(&mut evidence, "", true, &["Artist A", "Artist B"]);
    directory(&mut evidence, "Artist A", true, &["Collection"]);
    directory(&mut evidence, "Artist A/Collection", true, &["one.wav"]);
    directory(&mut evidence, "Artist B", false, &[]);
    evidence.issues.push(ScanIssue {
        path: "/music/Artist B".into(),
        reason: "Permission denied".into(),
    });
    let updated = reconcile(&old, vec![album("fresh-id", vec![one])], evidence);
    assert_eq!(updated.albums.len(), 1);
    assert_eq!(updated.albums[0].id.item, "fresh-id");
    assert_eq!(tracks(&updated).len(), 2);
    assert_eq!(
        tracks(&updated)
            .iter()
            .map(|track| &track.id.item)
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );
    assert!(tracks(&updated)[0].scan_status.is_none());
    assert!(tracks(&updated)[1].scan_status.is_some());
    assert_eq!(updated.scan.unwrap().removed, 0);
}

#[test]
fn real_folder_refresh_uses_enumeration_to_apply_deletions() {
    let directory = tempfile::tempdir().unwrap();
    let make_wav = |name: &str| {
        let mut wav = hound::WavWriter::create(
            directory.path().join(name),
            hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..128 {
            wav.write_sample(0i16).unwrap();
        }
        wav.finalize().unwrap();
    };
    make_wav("one.wav");
    make_wav("two.wav");
    let provider = crate::FilesystemProvider {
        root: directory.path().into(),
        preferences: Default::default(),
    };
    let original = provider
        .scan_with_coverage(&|| false, &mut |_| {})
        .unwrap()
        .reconcile(None);
    std::fs::remove_file(directory.path().join("two.wav")).unwrap();
    make_wav("three.wav");
    let updated = provider
        .scan_with_coverage(&|| false, &mut |_| {})
        .unwrap()
        .reconcile(Some(&original));
    let titles: BTreeSet<_> = tracks(&updated)
        .iter()
        .map(|track| track.title.as_str())
        .collect();
    assert_eq!(titles, BTreeSet::from(["one", "three"]));
    let summary = updated.scan.unwrap();
    assert_eq!(
        (summary.added, summary.removed, summary.unverified),
        (1, 1, 0)
    );
    assert!(!summary.incomplete);
}
