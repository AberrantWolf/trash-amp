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

impl Adapter {
    pub async fn apply_playlist_action(
        &self,
        key: ConnectionKey,
        action: PlaylistAction,
    ) -> Result<()> {
        let (collection, list, create, update) = match action {
            PlaylistAction::Retry => {
                for pending in self.pending_commands(key)? {
                    self.submit(pending).await?;
                }
                return Ok(());
            }
            PlaylistAction::Discard => return self.discard_commands(key),
            PlaylistAction::Create { collection, name } => (
                collection,
                None,
                Some(api::CreateCollectionList { kind: None, name }),
                None,
            ),
            PlaylistAction::Update { list, change } => {
                let change = match change {
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
        self.submit(ListCommand {
            key,
            collection,
            list,
            create,
            update,
            receipt: uuid::Uuid::new_v4().to_string(),
        })
        .await?;
        Ok(())
    }
}
