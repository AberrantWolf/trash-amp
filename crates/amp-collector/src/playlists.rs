//! Translate playlist intents into stable, replayable Collector commands.
use super::*;

#[derive(Clone, Copy)]
pub enum AppendScope {
    Album,
    Disc,
    Track,
}

pub enum PlaylistChange {
    Edit(api::CollectionListChange),
    Append {
        track: Box<Track>,
        scope: AppendScope,
    },
    /// Append a whole album by catalog identity, without loading its tracks.
    AppendItem {
        item: ids::ItemId,
        collection: ids::CollectionId,
    },
}

pub enum PlaylistAction {
    Create {
        collection: ids::CollectionId,
        name: String,
    },
    Update {
        list: api::CollectionListSummary,
        change: PlaylistChange,
    },
    Retry,
    Discard,
}

/// What the server accepted. Cached lists are patched from this rather than
/// refreshing the whole catalog after every edit.
#[derive(Clone, Debug)]
pub enum PlaylistOutcome {
    Created(api::CollectionListSummary),
    Updated {
        list: api::CollectionListSummary,
        change: api::CollectionListChange,
    },
    /// Replayed commands with unknown effects; a catalog refresh reconciles them.
    Retried,
    Discarded,
}

/// Apply an entry edit to loaded rows. Appends need server-presented entries,
/// so they leave the rows alone and the larger entry count triggers a reload.
pub fn apply_list_change(row: &mut api::CollectionListView, change: &api::CollectionListChange) {
    let items = &mut row.entries.items;
    match change {
        api::CollectionListChange::Remove { entry_id } => items.retain(|e| e.id != *entry_id),
        api::CollectionListChange::Move { entry_id, before } => {
            let Some(from) = items.iter().position(|e| e.id == *entry_id) else {
                return;
            };
            let entry = items.remove(from);
            let to = before
                .and_then(|before| items.iter().position(|e| e.id == before))
                .unwrap_or(items.len());
            items.insert(to, entry);
        }
        api::CollectionListChange::Rename { .. }
        | api::CollectionListChange::Delete
        | api::CollectionListChange::Append { .. } => {}
    }
}

/// Patch cached lists with an accepted outcome. Rows already at or past the
/// outcome's revision are newer and left untouched. Returns whether anything changed.
pub fn patch_lists(lists: &mut Vec<api::CollectionListView>, outcome: &PlaylistOutcome) -> bool {
    match outcome {
        PlaylistOutcome::Created(list) => {
            if lists.iter().any(|row| row.list.id == list.id) {
                return false;
            }
            lists.push(api::CollectionListView {
                list: list.clone(),
                entries: ids::Page {
                    items: Vec::new(),
                    next_cursor: None,
                },
            });
            true
        }
        PlaylistOutcome::Updated { list, change } => {
            let Some(index) = lists.iter().position(|row| row.list.id == list.id) else {
                return false;
            };
            if matches!(change, api::CollectionListChange::Delete) {
                lists.remove(index);
                return true;
            }
            let row = &mut lists[index];
            if row.list.revision.0 >= list.revision.0 {
                return false;
            }
            apply_list_change(row, change);
            row.list = list.clone();
            true
        }
        PlaylistOutcome::Retried | PlaylistOutcome::Discarded => false,
    }
}

/// The connection and collection a Collector track belongs to.
pub fn track_collection(track: &Track) -> Option<(ConnectionKey, ids::CollectionId)> {
    parse_remote(track).ok().map(|r| (r.key, r.collection))
}

/// Lists most recently created or added to on this device first, then the rest
/// newest first. List IDs are UUIDv7, so their order is their creation order.
pub fn by_recent_use<'a>(
    lists: impl IntoIterator<Item = &'a api::CollectionListView>,
    recent: &[ids::CollectionListId],
) -> Vec<&'a api::CollectionListView> {
    let mut lists = lists.into_iter().collect::<Vec<_>>();
    lists.sort_by_key(|list| {
        let used = recent.iter().position(|id| *id == list.list.id);
        (used.is_none(), used, std::cmp::Reverse(list.list.id))
    });
    lists
}

/// Remembered per connection on this device only; never sent to the server.
const RECENT_LISTS: usize = 20;

impl Adapter {
    pub fn recent_lists(&self, key: ConnectionKey) -> Vec<ids::CollectionListId> {
        self.get("recent_lists", &connection_id(key))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn note_recent_list(&self, key: ConnectionKey, list: ids::CollectionListId) {
        let mut recent = self.recent_lists(key);
        recent.retain(|id| *id != list);
        recent.insert(0, list);
        recent.truncate(RECENT_LISTS);
        // Ordering is a convenience; failing to save it must not fail the edit.
        let _ = self.put("recent_lists", &connection_id(key), &recent);
    }

    pub async fn apply_playlist_action(
        &self,
        key: ConnectionKey,
        action: PlaylistAction,
    ) -> Result<PlaylistOutcome> {
        let (collection, list, create, update) = match action {
            PlaylistAction::Retry => {
                for pending in self.pending_commands(key)? {
                    self.submit(pending).await?;
                }
                return Ok(PlaylistOutcome::Retried);
            }
            PlaylistAction::Discard => {
                self.discard_commands(key)?;
                return Ok(PlaylistOutcome::Discarded);
            }
            PlaylistAction::Create { collection, name } => {
                let name = name.trim().to_owned();
                anyhow::ensure!(!name.is_empty(), "Name the list");
                (
                    collection,
                    None,
                    Some(api::CreateCollectionList { kind: None, name }),
                    None,
                )
            }
            PlaylistAction::Update { list, change } => {
                let change = match change {
                    PlaylistChange::Edit(api::CollectionListChange::Rename { name }) => {
                        let name = name.trim().to_owned();
                        anyhow::ensure!(!name.is_empty(), "Name the list");
                        api::CollectionListChange::Rename { name }
                    }
                    PlaylistChange::Edit(change) => change,
                    PlaylistChange::Append { track, scope } => {
                        let remote = parse_remote(&track).context("Choose a Collector album")?;
                        anyhow::ensure!(
                            remote.key == key && remote.collection == list.collection_id,
                            "List belongs to another collection"
                        );
                        api::CollectionListChange::Append {
                            item_id: remote.item,
                            source_representation_id: (!matches!(scope, AppendScope::Album))
                                .then_some(remote.source),
                            track_position: matches!(scope, AppendScope::Track)
                                .then_some(remote.position),
                        }
                    }
                    PlaylistChange::AppendItem { item, collection } => {
                        anyhow::ensure!(
                            collection == list.collection_id,
                            "List belongs to another collection"
                        );
                        api::CollectionListChange::Append {
                            item_id: item,
                            source_representation_id: None,
                            track_position: None,
                        }
                    }
                };
                (
                    list.collection_id,
                    Some(list.id),
                    None,
                    Some(api::UpdateCollectionList {
                        expected_revision: list.revision,
                        change,
                    }),
                )
            }
        };
        anyhow::ensure!(
            self.pending_commands(key)?.is_empty(),
            "Retry or discard unresolved changes before making another change"
        );
        let change = update.as_ref().map(|update| update.change.clone());
        let summary = self
            .submit(ListCommand {
                key,
                collection,
                list,
                create,
                update,
                receipt: uuid::Uuid::new_v4().to_string(),
            })
            .await?;
        let outcome = match change {
            None => PlaylistOutcome::Created(summary),
            Some(change) => PlaylistOutcome::Updated {
                list: summary,
                change,
            },
        };
        // The server accepted the change; a cache write failure must not report
        // it as failed. The next catalog refresh reconciles the cache.
        let _ = self.update_snapshot(key, |snapshot| {
            patch_lists(&mut snapshot.lists, &outcome);
            Ok(())
        });
        match &outcome {
            PlaylistOutcome::Created(list)
            | PlaylistOutcome::Updated {
                list,
                change: api::CollectionListChange::Append { .. },
            } => self.note_recent_list(key, list.id),
            _ => {}
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: ids::ListEntryId, title: &str) -> api::PresentedListEntry {
        api::PresentedListEntry {
            source: None,
            audio: None,
            id,
            position: 0,
            item_id: None,
            title: title.into(),
            album: None,
            artist: None,
            disc_position: None,
            track_position: None,
            artwork: None,
            availability: api::ListEntryAvailability::Entry,
        }
    }

    fn list(revision: u64, titles: &[&str]) -> api::CollectionListView {
        api::CollectionListView {
            list: api::CollectionListSummary {
                id: ids::CollectionListId::new(),
                collection_id: ids::CollectionId::new(),
                revision: ids::Revision(revision),
                name: "Road music".into(),
                kind: ids::CollectionListKind::MusicTracks,
                entry_count: titles.len() as u32,
                can_edit: true,
            },
            entries: ids::Page {
                items: titles
                    .iter()
                    .map(|t| entry(ids::ListEntryId::new(), t))
                    .collect(),
                next_cursor: None,
            },
        }
    }

    fn titles(row: &api::CollectionListView) -> Vec<&str> {
        row.entries.items.iter().map(|e| e.title.as_str()).collect()
    }

    #[test]
    fn accepted_edits_patch_rows_without_a_reload() {
        let mut lists = vec![list(1, &["a", "b", "c"])];
        let mut summary = lists[0].list.clone();
        summary.revision = ids::Revision(2);
        summary.entry_count = 2;
        let b = lists[0].entries.items[1].id;
        let removed = PlaylistOutcome::Updated {
            list: summary.clone(),
            change: api::CollectionListChange::Remove { entry_id: b },
        };
        assert!(patch_lists(&mut lists, &removed));
        assert_eq!(titles(&lists[0]), ["a", "c"]);
        assert_eq!(lists[0].list.revision, ids::Revision(2));
        // A replayed or stale outcome never applies twice.
        assert!(!patch_lists(&mut lists, &removed));

        let a = lists[0].entries.items[0].id;
        summary.revision = ids::Revision(3);
        assert!(patch_lists(
            &mut lists,
            &PlaylistOutcome::Updated {
                list: summary.clone(),
                change: api::CollectionListChange::Move {
                    entry_id: a,
                    before: None
                },
            }
        ));
        assert_eq!(titles(&lists[0]), ["c", "a"]);

        summary.revision = ids::Revision(4);
        summary.entry_count = 5;
        assert!(patch_lists(
            &mut lists,
            &PlaylistOutcome::Updated {
                list: summary.clone(),
                change: api::CollectionListChange::Append {
                    item_id: ids::ItemId::new(),
                    source_representation_id: None,
                    track_position: None,
                },
            }
        ));
        // Appended rows load from the server; the count says they are missing.
        assert_eq!(lists[0].entries.items.len(), 2);
        assert_eq!(lists[0].list.entry_count, 5);

        summary.revision = ids::Revision(5);
        assert!(patch_lists(
            &mut lists,
            &PlaylistOutcome::Updated {
                list: summary.clone(),
                change: api::CollectionListChange::Delete,
            }
        ));
        assert!(lists.is_empty());
        assert!(patch_lists(
            &mut lists,
            &PlaylistOutcome::Created(summary.clone())
        ));
        assert!(!patch_lists(&mut lists, &PlaylistOutcome::Created(summary)));
        assert_eq!(lists.len(), 1);
    }

    #[test]
    fn recently_used_lists_come_first_then_newest_created() {
        let older = list(1, &[]);
        let newer = list(1, &[]);
        let used = list(1, &[]);
        let names = |lists: Vec<&api::CollectionListView>| {
            lists.iter().map(|l| l.list.id).collect::<Vec<_>>()
        };
        let all = [&older, &newer, &used];
        assert_eq!(
            names(by_recent_use(all, &[])),
            [used.list.id, newer.list.id, older.list.id]
        );
        assert_eq!(
            names(by_recent_use(all, &[older.list.id])),
            [older.list.id, used.list.id, newer.list.id]
        );
    }

    #[test]
    fn moving_before_an_entry_matches_the_server_order() {
        let mut row = list(1, &["a", "b", "c", "d"]);
        let (b, d) = (row.entries.items[1].id, row.entries.items[3].id);
        apply_list_change(
            &mut row,
            &api::CollectionListChange::Move {
                entry_id: b,
                before: Some(d),
            },
        );
        assert_eq!(titles(&row), ["a", "c", "b", "d"]);
    }
}
