//! Reconcile only absence supported by directory coverage. No filesystem I/O.
use crate::coverage::Coverage;
use amp_core::{Album, ItemId, Library, MediaAccess, ScanIssue, ScanStatus, ScanSummary, Track};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub struct ScanResult {
    pub library: Library,
    /// None is reserved for providers/backends returning an authoritative snapshot.
    pub coverage: Option<Coverage>,
}
impl ScanResult {
    pub fn complete(library: Library) -> Self {
        Self {
            library,
            coverage: None,
        }
    }
    pub fn reconcile(self, previous: Option<&Library>) -> Library {
        let Self {
            mut library,
            coverage,
        } = self;
        let Some(mut coverage) = coverage else {
            return library;
        };
        let previous = previous.filter(|old| old.id == library.id);
        let old_identity = previous.and_then(|old| old.scan.as_ref()?.source_identity.as_ref());
        if let (Some(previous), Some(old), Some(current)) =
            (previous, old_identity, &coverage.source_identity)
            && old != current
        {
            // Do not establish a new baseline from an empty unmounted directory:
            // even a second refresh must still require the original source.
            let mut unchanged = previous.clone();
            let reason =
                "The original source is disconnected or a different volume is mounted here"
                    .to_owned();
            for track in unchanged
                .albums
                .iter_mut()
                .flat_map(|album| &mut album.tracks)
            {
                track.scan_status = Some(ScanStatus::Unverified {
                    path: library.root.clone(),
                    reason: reason.clone(),
                });
            }
            unchanged.scan = Some(ScanSummary {
                source_identity: Some(old.clone()),
                mounts: previous
                    .scan
                    .as_ref()
                    .map(|scan| scan.mounts.clone())
                    .unwrap_or_default(),
                incomplete: true,
                source_disconnected: true,
                added: 0,
                removed: 0,
                unverified: unchanged
                    .albums
                    .iter()
                    .map(|album| album.tracks.len())
                    .sum(),
                issues: vec![ScanIssue {
                    path: library.root,
                    reason,
                }],
            });
            return unchanged;
        }

        // A selected folder can contain a separately mounted share or drive.
        // Losing that child source must invalidate deletion evidence even when
        // the selected root itself is still healthy.
        let mut mounts = previous
            .and_then(|old| old.scan.as_ref())
            .map(|scan| scan.mounts.clone())
            .unwrap_or_default();
        let mut changed_mounts = Vec::new();
        for (path, identity) in &mounts {
            if coverage.mounts.get(Path::new(path)) != Some(identity) {
                changed_mounts.push(PathBuf::from(path));
                coverage.source_disconnected = true;
                coverage.issues.push(ScanIssue {
                    path: path.clone(),
                    reason: "This mounted source is disconnected or has changed".into(),
                });
            }
        }
        for (path, identity) in &coverage.mounts {
            if changed_mounts
                .iter()
                .any(|changed| path.starts_with(changed))
            {
                continue;
            }
            mounts
                .entry(path.to_string_lossy().into_owned())
                .or_insert_with(|| identity.clone());
        }

        // Results from a replacement volume are not changes to the original
        // library, even when their paths collide with cached track identities.
        // Discard that subtree and keep its last-known catalog instead.
        for album in &mut library.albums {
            album.tracks.retain(|track| {
                !track_path(track).is_some_and(|path| {
                    changed_mounts
                        .iter()
                        .any(|changed| path.starts_with(changed))
                })
            });
        }
        library.albums.retain(|album| !album.tracks.is_empty());
        let issues_by_path: BTreeMap<_, _> = coverage
            .issues
            .iter()
            .map(|issue| (PathBuf::from(&issue.path), issue))
            .collect();

        let old_keys: BTreeSet<_> = previous
            .into_iter()
            .flat_map(|old| &old.albums)
            .flat_map(|album| &album.tracks)
            .map(|track| key(&track.id))
            .collect();
        // A recognizable new file may have a filename-only fallback. When we
        // already know that file, its last successfully read metadata is better.
        for album in &mut library.albums {
            album
                .tracks
                .retain(|track| track.scan_status.is_none() || !old_keys.contains(&key(&track.id)));
        }
        library.albums.retain(|album| !album.tracks.is_empty());
        let mut fresh_keys: BTreeSet<_> = library
            .albums
            .iter()
            .flat_map(|album| &album.tracks)
            .map(|track| key(&track.id))
            .collect();
        let mut album_indices: BTreeMap<_, _> = library
            .albums
            .iter()
            .enumerate()
            .map(|(index, album)| (key(&album.id), index))
            .collect();
        let mut track_albums: BTreeMap<_, _> = library
            .albums
            .iter()
            .enumerate()
            .flat_map(|(index, album)| {
                album
                    .tracks
                    .iter()
                    .map(move |track| (key(&track.id), index))
            })
            .collect();
        let mut removed = 0;
        if let Some(previous) = previous {
            for old_album in &previous.albums {
                let mut unchecked = Vec::new();
                for track in &old_album.tracks {
                    if fresh_keys.contains(&key(&track.id)) {
                        continue;
                    }
                    let path = track_path(track);
                    if path
                        .as_ref()
                        .is_some_and(|path| coverage.removal_proven(path))
                    {
                        removed += 1;
                        continue;
                    }
                    let mut track = track.clone();
                    track.scan_status = Some(uncertainty(
                        &coverage,
                        &issues_by_path,
                        &track,
                        path.as_deref(),
                    ));
                    fresh_keys.insert(key(&track.id));
                    unchecked.push(track);
                }
                if unchecked.is_empty() {
                    continue;
                }
                let target = album_indices.get(&key(&old_album.id)).copied().or_else(|| {
                    let mut overlaps = BTreeMap::<usize, usize>::new();
                    for track in &old_album.tracks {
                        if let Some(index) = track_albums.get(&key(&track.id)) {
                            *overlaps.entry(*index).or_default() += 1;
                        }
                    }
                    overlaps
                        .into_iter()
                        .max_by_key(|(_, count)| *count)
                        .map(|(index, _)| index)
                });
                let index = match target {
                    Some(index) => index,
                    None => {
                        let mut album = old_album.clone();
                        album.tracks.clear();
                        library.albums.push(album);
                        let index = library.albums.len() - 1;
                        album_indices.insert(key(&old_album.id), index);
                        index
                    }
                };
                for track in &unchecked {
                    track_albums.insert(key(&track.id), index);
                }
                append_unchecked(&mut library.albums[index], old_album, unchecked);
            }
        }
        library.albums.retain(|album| !album.tracks.is_empty());
        for album in &mut library.albums {
            album.tracks.sort_by(|a, b| {
                (a.disc, a.number, &a.id.item).cmp(&(b.disc, b.number, &b.id.item))
            });
        }
        library.albums.sort_by(|a, b| {
            (&a.artist, &a.title, &a.id.item).cmp(&(&b.artist, &b.title, &b.id.item))
        });
        let unverified = library
            .albums
            .iter()
            .flat_map(|album| &album.tracks)
            .filter(|track| track.scan_status.is_some())
            .count();
        let added = fresh_keys.difference(&old_keys).count();
        let mut warnings: BTreeSet<_> = library.warnings.iter().cloned().collect();
        for issue in &coverage.issues {
            let warning = format!("{}: {}", issue.path, issue.reason);
            if warnings.insert(warning.clone()) {
                library.warnings.push(warning);
            }
        }
        library.scan = Some(ScanSummary {
            incomplete: coverage.source_disconnected
                || !coverage.issues.is_empty()
                || unverified > 0,
            source_disconnected: coverage.source_disconnected,
            source_identity: coverage.source_identity.or_else(|| old_identity.cloned()),
            mounts,
            added,
            removed,
            unverified,
            issues: coverage.issues,
        });
        library
    }
}
fn key(id: &ItemId) -> (String, String, String) {
    (id.library.clone(), id.provider.clone(), id.item.clone())
}
fn track_path(track: &Track) -> Option<PathBuf> {
    match &track.access {
        MediaAccess::File { locator } => Some(locator.into()),
        MediaAccess::DiscTrack { cue, .. } => Some(cue.into()),
        MediaAccess::RedumperTrack { anchor, .. } => {
            Some(Path::new(anchor).with_extension("scram"))
        }
        MediaAccess::Managed { .. } => None,
    }
}
fn uncertainty(
    coverage: &Coverage,
    issues: &BTreeMap<PathBuf, &ScanIssue>,
    track: &Track,
    path: Option<&Path>,
) -> ScanStatus {
    if let Some((path, reason)) =
        path.and_then(|path| coverage.failures.get(path).map(|reason| (path, reason)))
    {
        return if matches!(track.access, MediaAccess::File { .. }) {
            ScanStatus::MetadataUnreadable {
                reason: reason.clone(),
            }
        } else {
            ScanStatus::Unverified {
                path: path.to_string_lossy().into(),
                reason: reason.clone(),
            }
        };
    }
    let issue = path.and_then(|path| {
        path.ancestors()
            .find_map(|parent| issues.get(parent).copied())
    });
    ScanStatus::Unverified {
        path: issue
            .map(|issue| issue.path.clone())
            .unwrap_or_else(|| coverage.root.to_string_lossy().into()),
        reason: issue.map(|issue| issue.reason.clone()).unwrap_or_else(|| {
            if coverage.source_disconnected {
                "The source disconnected during refresh".into()
            } else {
                "This part of the folder couldn’t be checked".into()
            }
        }),
    }
}
fn append_unchecked(target: &mut Album, old: &Album, tracks: Vec<Track>) {
    // Readable tracks can still have generated album labels. A partial refresh
    // must not replace previously known names with those placeholders, since
    // enrichment deliberately avoids reopening unchecked albums.
    if matches!(&target.metadata.names.title, amp_core::NameOrigin::Generated(value) if value == &target.title)
    {
        target.title.clone_from(&old.title);
        target
            .metadata
            .names
            .title
            .clone_from(&old.metadata.names.title);
    }
    if matches!(&target.metadata.names.artist, amp_core::NameOrigin::Generated(value) if value == &target.artist)
    {
        target.artist.clone_from(&old.artist);
        target
            .metadata
            .names
            .artist
            .clone_from(&old.metadata.names.artist);
    }
    for track in tracks {
        if let Some(names) = old.metadata.tracks.get(&track.id.item) {
            target
                .metadata
                .tracks
                .insert(track.id.item.clone(), names.clone());
        }
        target.tracks.push(track);
    }
}

#[cfg(test)]
mod tests;
