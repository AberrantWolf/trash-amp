//! Slint presentation adapter. Every operation runs on a worker; browsing and playback have separate lifetimes.
use super::*;
use amp_app::{browse::SelectionRead, catalog::Generation};
#[cfg(test)]
use amp_collector::Snapshot;
use amp_collector::{
    Adapter, AppendScope, CatalogView, ConnectionKey, ConnectionState, Download, DownloadStatus,
    PlaybackFormat, PlaylistAction, PlaylistChange, PlaylistOutcome, PlaylistTracks,
    SavedConnection, SourceView, api, apply_list_change, by_recent_use, ids, patch_lists,
    track_collection,
};
use anyhow::Context;
use std::path::Path;
struct View {
    connections: Vec<SavedConnection>,
    selected: Option<ConnectionKey>,
    snapshot: Option<CatalogView>,
    list: Option<ids::CollectionListId>,
    collection: Option<ids::CollectionId>,
    online: bool,
    connection: ConnectionState,
    operation: Option<(u64, u64)>,
    /// The open list's rows, edited in place for removals and moves.
    list_rows: RefCell<Option<artwork::Rows>>,
    /// The open list as it was before an edit shown ahead of the server's answer.
    optimistic: Option<api::CollectionListView>,
    /// Lists most recently created or added to on this device, newest first.
    recent: Vec<ids::CollectionListId>,
}
impl View {
    fn selected_list(&self) -> Option<&api::CollectionListView> {
        self.selected_list_index().map(|(_, list)| list)
    }
    fn selected_list_index(&self) -> Option<(usize, &api::CollectionListView)> {
        let id = self.list?;
        self.snapshot
            .as_ref()?
            .lists
            .iter()
            .enumerate()
            .find(|(_, l)| l.list.id == id)
    }
    fn list_mut(&mut self, id: ids::CollectionListId) -> Option<&mut api::CollectionListView> {
        self.snapshot
            .as_mut()?
            .lists
            .iter_mut()
            .find(|l| l.list.id == id)
    }
}
struct Connected {
    connections: Vec<SavedConnection>,
}
fn open_source(catalog: &Catalog, version: u64, key: ConnectionKey) {
    catalog.open_source_at(version, amp_collector::source_id(key), true);
}
enum Event {
    Connected(u64, anyhow::Result<Box<Connected>>),
    Approval(u64, String),
    OpenFailed(u64, String),
    Mutation(u64, ConnectionKey, Purpose, Applied),
    Downloads(anyhow::Result<Vec<Download>>),
    DownloadMessage(String),
    PlaybackFormatSaved(anyhow::Result<()>),
    AlbumLoaded(
        u64,
        ConnectionKey,
        amp_core::ItemId,
        u64,
        anyhow::Result<Box<amp_core::Album>>,
    ),
    ListLoaded(
        u64,
        ConnectionKey,
        ids::CollectionListId,
        u64,
        anyhow::Result<Box<api::CollectionListView>>,
    ),
    Removed(ConnectionKey, anyhow::Result<()>),
    ConnectionChecked(u64, ConnectionKey, ConnectionState),
}
pub struct Controller {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    ui: slint::Weak<PlayerWindow>,
    state: Rc<RefCell<State>>,
    adapter: Arc<Adapter>,
    view: Rc<RefCell<View>>,
    generation: Generation,
    connection_probe: Option<(u64, ConnectionKey, std::time::Instant, bool)>,
    open_url: crate::UrlOpener,
    source_revision: (u64, u64),
    source_projection: Option<mpsc::Receiver<SourceProjection>>,
    album_request: SelectionRead<(u64, ConnectionKey, amp_core::ItemId)>,
    list_request: SelectionRead<(u64, ConnectionKey, ids::CollectionListId)>,
    download_records: Rc<RefCell<Vec<Download>>>,
}
struct SourceProjection {
    generation: u64,
    revision: u64,
    source: CatalogSource,
    view: anyhow::Result<SourceView>,
}
fn connection_kind(state: ConnectionState) -> ConnectionKind {
    match state {
        ConnectionState::Disconnected => ConnectionKind::Disconnected,
        ConnectionState::Connected => ConnectionKind::Checking,
        ConnectionState::Public => ConnectionKind::Public,
        ConnectionState::Lan => ConnectionKind::Lan,
        ConnectionState::Direct => ConnectionKind::Direct,
        ConnectionState::StreamingUnavailable => ConnectionKind::Unavailable,
    }
}
pub fn queue_label(t: &Track) -> String {
    if t.artist.is_empty() {
        t.title.clone()
    } else {
        format!("{} · {}", t.title, t.artist)
    }
}
fn connection_error(ui: &PlayerWindow, message: String) {
    ui.set_connection_status(message.clone().into());
    ui.set_error_title("Connection failed".into());
    ui.set_error_message(message.into());
}
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}
fn install(ui: &PlayerWindow, state: &Rc<RefCell<State>>, view: &View, downloads: &[Download]) {
    let mut s = state.borrow_mut();
    let same = view
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| s.library.id == snapshot.library.id);
    let selected = if same {
        s.selected
            .and_then(|i| s.library.albums.get(i))
            .map(|a| a.id.clone())
    } else {
        None
    };
    let showing_library = ui.get_show_library();
    let showing_queue = ui.get_show_queue();
    let selected_track = s.selected.filter(|_| same).and_then(|index| {
        usize::try_from(ui.get_selected_track())
            .ok()
            .and_then(|row| {
                displayed_tracks(&s, index)
                    .get(row)
                    .map(|track| track.id.clone())
            })
    });
    let track_y = ui.get_track_y();
    if let Some(snapshot) = &view.snapshot {
        s.library = snapshot.library_for(view.collection);
        s.artwork = snapshot.artwork.clone();
        ui.set_connection_status(
            format!(
                "{} · last refresh {} seconds ago",
                if view.online {
                    "Connected"
                } else {
                    "Cached / offline"
                },
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_sub(snapshot.updated)
            )
            .into(),
        );
    } else {
        s.library = Arc::default();
        s.artwork = Arc::default();
    }
    let album_keys = s
        .library
        .albums
        .iter()
        .map(album_key)
        .collect::<std::collections::BTreeSet<_>>();
    s.selected_sources.retain(|key, _| album_keys.contains(key));
    s.selected = None;
    albums(ui, &s);
    if let Some(index) = selected.and_then(|id| s.library.albums.iter().position(|a| a.id == id)) {
        select(ui, &mut s, index);
        ui.set_show_library(showing_library);
        ui.set_show_queue(showing_queue);
        if let Some(row) = selected_track.and_then(|id| {
            displayed_tracks(&s, index)
                .iter()
                .position(|track| track.id == id)
        }) {
            ui.set_selected_track(row as i32);
            ui.set_track_y(track_y);
        }
    }
    browse::set_search_scope(ui, &mut s, view.collection.map(|id| id.to_string()));
    browse::request_rebuild(ui, &mut s, !same);
    if s.selected.is_none() {
        ui.set_album_art(slint::Image::default());
    }
    ui.set_collector_active(view.selected.is_some());
    ui.set_collector_online(view.online);
    ui.set_connection_kind(connection_kind(view.connection));
    ui.set_collections(ModelRc::new(VecModel::from(
        std::iter::once(SharedString::from("All music collections"))
            .chain(
                view.snapshot
                    .as_ref()
                    .into_iter()
                    .flat_map(|s| s.collections.iter().map(|c| SharedString::from(&c.name))),
            )
            .collect::<Vec<_>>(),
    )));
    render_list(ui, view, downloads);
}
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}
fn entry_artwork(snapshot: &CatalogView, entry: &api::PresentedListEntry) -> Option<PathBuf> {
    entry
        .item_id
        .and_then(|id| snapshot.artwork.get(&id.to_string()))
        .map(PathBuf::from)
}
fn list_subtitle(snapshot: &CatalogView, list: &api::CollectionListView) -> String {
    let mut text = plural(list.list.entry_count as usize, "track");
    if snapshot.collections.len() > 1
        && let Some(collection) = snapshot
            .collections
            .iter()
            .find(|c| c.id == list.list.collection_id)
    {
        text = format!("{text} · {}", collection.name);
    }
    text
}
/// One row per list, covered by the first entry that has artwork.
fn list_cards<'a>(
    snapshot: &CatalogView,
    lists: impl Iterator<Item = &'a api::CollectionListView>,
) -> ModelRc<BrowserRow> {
    let (rows, paths): (Vec<_>, Vec<_>) = lists
        .map(|list| {
            (
                BrowserRow {
                    kind: 1,
                    title: list.list.name.clone().into(),
                    subtitle: list_subtitle(snapshot, list).into(),
                    status: if list.list.can_edit { "" } else { "Read only" }.into(),
                    ..Default::default()
                },
                list.entries
                    .items
                    .iter()
                    .find_map(|entry| entry_artwork(snapshot, entry)),
            )
        })
        .collect();
    artwork::model(rows, paths)
}
/// How many lists a right-click "Add to list" menu offers directly.
const MENU_LISTS: usize = 5;
/// Editable lists for right-click menus, most recently used first.
fn menu_lists(view: &View) -> Vec<&api::CollectionListView> {
    let Some(snapshot) = view.snapshot.as_ref() else {
        return Vec::new();
    };
    let mut lists = by_recent_use(
        snapshot.lists.iter().filter(|l| l.list.can_edit),
        &view.recent,
    );
    lists.truncate(MENU_LISTS);
    lists
}
/// Everything on the Lists page except the open list's rows.
fn render_lists(ui: &PlayerWindow, view: &View) {
    let several = view
        .snapshot
        .as_ref()
        .is_some_and(|s| s.collections.len() > 1);
    ui.set_recent_lists(ModelRc::new(VecModel::from(
        menu_lists(view)
            .iter()
            .map(|list| {
                let collection = several
                    .then(|| {
                        view.snapshot
                            .as_ref()?
                            .collections
                            .iter()
                            .find(|c| c.id == list.list.collection_id)
                    })
                    .flatten();
                SharedString::from(match collection {
                    Some(collection) => format!("{} · {}", list.list.name, collection.name),
                    None => list.list.name.clone(),
                })
            })
            .collect::<Vec<_>>(),
    )));
    let snapshot = view.snapshot.as_ref();
    let editable = |collection: &api::ClientCollection| collection.can_edit_playlists;
    ui.set_can_add_to_list(
        view.online && snapshot.is_some_and(|s| s.collections.iter().any(editable)),
    );
    ui.set_can_create_list(snapshot.is_some_and(|s| {
        s.collections
            .iter()
            .any(|c| editable(c) && view.collection.is_none_or(|id| c.id == id))
    }));
    let Some(snapshot) = snapshot else {
        ui.set_list_cards(ModelRc::default());
        ui.set_open_list(-1);
        return;
    };
    ui.set_list_cards(list_cards(snapshot, snapshot.lists.iter()));
    let Some((index, list)) = view.selected_list_index() else {
        ui.set_open_list(-1);
        ui.set_list_editable(false);
        return;
    };
    ui.set_open_list(index as i32);
    ui.set_open_list_title(list.list.name.clone().into());
    let mut detail = list_subtitle(snapshot, list);
    if !list.list.can_edit {
        detail += " · Read only";
    }
    ui.set_open_list_detail(detail.into());
    ui.set_open_list_art(
        list.entries
            .items
            .iter()
            .position(|entry| entry_artwork(snapshot, entry).is_some())
            .map_or(-1, |i| i as i32),
    );
    ui.set_list_editable(list.list.can_edit);
}
fn render_list(ui: &PlayerWindow, view: &View, downloads: &[Download]) {
    ui.set_list_entry(-1);
    render_lists(ui, view);
    let (Some(snapshot), Some(list)) = (view.snapshot.as_ref(), view.selected_list()) else {
        view.list_rows.replace(None);
        ui.set_list_rows(ModelRc::default());
        return;
    };
    let tracks = view
        .selected
        .map(|key| PlaylistTracks::new(key, list, downloads));
    let paths = list
        .entries
        .items
        .iter()
        .map(|e| entry_artwork(snapshot, e))
        .collect();
    let rows = artwork::Rows::new(
        list.entries
            .items
            .iter()
            .map(|e| {
                let status = if (view.online
                    && e.audio
                        .as_ref()
                        .is_some_and(|audio| audio.can_stream_original))
                    || tracks.as_ref().is_some_and(|tracks| tracks.is_retained(e))
                {
                    ""
                } else {
                    match e.availability {
                        api::ListEntryAvailability::Available { .. } => "",
                        api::ListEntryAvailability::Preparing => "Preparing",
                        api::ListEntryAvailability::NeedsPreparation => "Needs preparation",
                        api::ListEntryAvailability::Entry => "Entry",
                        _ => "Unavailable",
                    }
                };
                let subtitle = [e.artist.as_deref(), e.album.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                BrowserRow {
                    kind: 1,
                    title: e.title.clone().into(),
                    artist: e.artist.as_deref().unwrap_or("").into(),
                    subtitle: subtitle.into(),
                    status: status.into(),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>(),
        paths,
    );
    ui.set_list_rows(rows.model());
    view.list_rows.replace(Some(rows));
}
fn playlist_playable(track: &Track, online: bool) -> bool {
    track.availability == amp_core::Availability::Downloaded
        || (online && track.availability == amp_core::Availability::Available)
}
/// Music chosen for "Add to list", kept while the picker is open.
#[derive(Clone)]
enum AddTarget {
    Album {
        item: ids::ItemId,
        collection: ids::CollectionId,
        /// The selected disc's first track, when the album has several discs.
        disc: Option<Box<Track>>,
    },
    /// Tracks from one collection, added in order.
    Tracks(Vec<Track>),
}
impl AddTarget {
    fn changes(&self, disc_only: bool) -> Vec<PlaylistChange> {
        match self {
            Self::Album {
                disc: Some(track), ..
            } if disc_only => vec![PlaylistChange::Append {
                track: track.clone(),
                scope: AppendScope::Disc,
            }],
            Self::Album {
                item, collection, ..
            } => vec![PlaylistChange::AppendItem {
                item: *item,
                collection: *collection,
            }],
            Self::Tracks(tracks) => tracks
                .iter()
                .map(|track| PlaylistChange::Append {
                    track: Box::new(track.clone()),
                    scope: AppendScope::Track,
                })
                .collect(),
        }
    }
    fn collection(&self) -> Option<ids::CollectionId> {
        match self {
            Self::Album { collection, .. } => Some(*collection),
            Self::Tracks(tracks) => tracks
                .first()
                .and_then(track_collection)
                .map(|(_, collection)| collection),
        }
    }
}
#[derive(Default)]
struct AddPicker {
    target: Option<(AddTarget, String)>,
    lists: Vec<ids::CollectionListId>,
}
fn album_target(
    view: &View,
    album: &amp_core::Album,
    disc: Option<Box<Track>>,
) -> Option<AddTarget> {
    let snapshot = view.snapshot.as_ref()?;
    let item = album.id.item.parse().ok()?;
    let collection = snapshot
        .album_collections
        .get(&album.id.item)
        .copied()
        .or_else(|| {
            album
                .tracks
                .first()
                .and_then(track_collection)
                .map(|(_, collection)| collection)
        })?;
    Some(AddTarget::Album {
        item,
        collection,
        disc,
    })
}
/// Resolve what an "Add to list" action refers to, with a description for messages.
fn add_target(
    ui: &PlayerWindow,
    state: &State,
    view: &View,
    source: AddSource,
    row: i32,
) -> Option<(AddTarget, String)> {
    let describe = |title: &str, artist: &str| {
        if artist.is_empty() {
            format!("“{title}”")
        } else {
            format!("“{title}” by {artist}")
        }
    };
    let track = |track: Track| {
        let text = describe(&track.title, &track.artist);
        Some((AddTarget::Tracks(vec![track]), text))
    };
    match source {
        AddSource::Album => {
            let index = state.selected?;
            let album = state.library.albums.get(index)?;
            let disc = ui
                .get_show_disc_selector()
                .then(|| {
                    displayed_tracks(state, index)
                        .into_iter()
                        .next()
                        .map(Box::new)
                })
                .flatten();
            let target = album_target(view, album, disc)?;
            Some((target, describe(&album.title, &album.artist)))
        }
        AddSource::Track => track(
            displayed_tracks(state, state.selected?)
                .into_iter()
                .nth(usize::try_from(row).ok()?)?,
        ),
        AddSource::Browser | AddSource::Detail => {
            let target = browse::row_target(
                state,
                source == AddSource::Detail,
                usize::try_from(row).ok()?,
            )?;
            match target {
                amp_core::browse::Target::Album(id) => {
                    let album = state.library.albums.iter().find(|a| a.id == id)?;
                    let target = album_target(view, album, None)?;
                    Some((target, describe(&album.title, &album.artist)))
                }
                target @ amp_core::browse::Target::Track { .. } => {
                    track(browse::resolve_track(state, &target)?)
                }
                amp_core::browse::Target::Artist(_) => None,
            }
        }
        AddSource::Queue => {
            // The playing track and everything after it, from this server's
            // first collection in the queue; a list holds one collection's music.
            let key = view.selected?;
            let playback = state.playback.snapshot();
            let queue = playback
                .current
                .iter()
                .chain(playback.up_next.iter())
                .collect::<Vec<_>>();
            let (_, collection) = queue
                .iter()
                .filter_map(|t| track_collection(t))
                .find(|(owner, _)| *owner == key)?;
            let tracks = queue
                .iter()
                .filter(|t| track_collection(t) == Some((key, collection)))
                .map(|t| (*t).clone())
                .collect::<Vec<_>>();
            let left_out = queue.len() - tracks.len();
            let mut text = format!("{} from up next", plural(tracks.len(), "track"));
            if left_out > 0 {
                text += &format!(" ({left_out} from elsewhere left out)");
            }
            Some((AddTarget::Tracks(tracks), text))
        }
    }
}
/// What a running list change is for, so its result can be reported in place.
enum Purpose {
    Edit,
    /// Adding the described music from the library.
    Add(String),
}
fn needs_resync(outcomes: &[PlaylistOutcome]) -> bool {
    outcomes
        .iter()
        .any(|o| matches!(o, PlaylistOutcome::Retried | PlaylistOutcome::Discarded))
}
type StartJob = dyn Fn(&PlayerWindow, ConnectionKey, Job, Purpose);
/// Adds a target, described by the string, to a list; true adds only the selected disc.
type StartAdd = dyn Fn(&PlayerWindow, AddTarget, String, AddTo, bool);
enum AddTo {
    Existing(api::CollectionListSummary),
    New {
        collection: ids::CollectionId,
        name: String,
    },
}
enum Job {
    Single(PlaylistAction),
    /// Append in order, each change at the revision the previous one produced.
    Add {
        list: AddTo,
        changes: Vec<PlaylistChange>,
    },
}
/// Outcomes the server accepted, and the error that stopped the job, if any.
struct Applied {
    outcomes: Vec<PlaylistOutcome>,
    error: Option<anyhow::Error>,
}
async fn run_job(adapter: &Adapter, key: ConnectionKey, job: Job) -> Applied {
    let mut outcomes = Vec::new();
    let error = match job {
        Job::Single(action) => match adapter.apply_playlist_action(key, action).await {
            Ok(outcome) => {
                outcomes.push(outcome);
                None
            }
            Err(error) => Some(error),
        },
        Job::Add { list, changes } => 'add: {
            let mut list = match list {
                AddTo::Existing(list) => list,
                AddTo::New { collection, name } => match adapter
                    .apply_playlist_action(key, PlaylistAction::Create { collection, name })
                    .await
                {
                    Ok(created) => {
                        let PlaylistOutcome::Created(list) = &created else {
                            unreachable!("creating a list reports its summary")
                        };
                        let list = list.clone();
                        outcomes.push(created);
                        list
                    }
                    Err(error) => break 'add Some(error),
                },
            };
            for change in changes {
                let action = PlaylistAction::Update {
                    list: list.clone(),
                    change,
                };
                match adapter.apply_playlist_action(key, action).await {
                    Ok(outcome) => {
                        if let PlaylistOutcome::Updated { list: next, .. } = &outcome {
                            list = next.clone();
                        }
                        outcomes.push(outcome);
                    }
                    Err(error) => break 'add Some(error),
                }
            }
            None
        }
    };
    Applied { outcomes, error }
}
fn playlist_request(
    view: &View,
    action: ListAction,
    entry: i32,
    name: &str,
) -> anyhow::Result<(ConnectionKey, PlaylistAction)> {
    let key = view.selected.context("Select server")?;
    if action == ListAction::Discard {
        return Ok((key, PlaylistAction::Discard));
    }
    anyhow::ensure!(
        view.online,
        "Editing lists needs a connection to the server"
    );
    if action == ListAction::Retry {
        return Ok((key, PlaylistAction::Retry));
    }
    if action == ListAction::Create {
        let snapshot = view.snapshot.as_ref().context("Refresh server")?;
        let collection = snapshot
            .collections
            .iter()
            .find(|collection| {
                collection.can_edit_playlists
                    && view.collection.is_none_or(|id| collection.id == id)
            })
            .context("Select an editable collection")?;
        return Ok((
            key,
            PlaylistAction::Create {
                collection: collection.id,
                name: name.to_owned(),
            },
        ));
    }
    let list = view.selected_list().context("Open a list")?;
    let change = match action {
        ListAction::Rename => PlaylistChange::Edit(api::CollectionListChange::Rename {
            name: name.to_owned(),
        }),
        ListAction::Delete => PlaylistChange::Edit(api::CollectionListChange::Delete),
        ListAction::RemoveEntry | ListAction::MoveUp | ListAction::MoveDown => {
            let i = usize::try_from(entry).context("Choose an entry")?;
            let entries = &list.entries.items;
            let entry = entries.get(i).context("Choose an entry")?;
            PlaylistChange::Edit(if action == ListAction::RemoveEntry {
                api::CollectionListChange::Remove { entry_id: entry.id }
            } else {
                let before = if action == ListAction::MoveUp {
                    entries.get(i.checked_sub(1).context("Already first")?)
                } else {
                    anyhow::ensure!(i + 1 < entries.len(), "Already last");
                    entries.get(i + 2)
                };
                api::CollectionListChange::Move {
                    entry_id: entry.id,
                    before: before.map(|row| row.id),
                }
            })
        }
        ListAction::Create | ListAction::Retry | ListAction::Discard => unreachable!(),
    };
    Ok((
        key,
        PlaylistAction::Update {
            list: list.list.clone(),
            change,
        },
    ))
}
fn sync_banner(ui: &PlayerWindow, adapter: &Adapter, key: Option<ConnectionKey>) {
    let pending = key.is_some_and(|key| {
        adapter
            .pending_commands(key)
            .is_ok_and(|pending| !pending.is_empty())
    });
    ui.set_list_sync_pending(pending);
    if pending {
        ui.set_list_sync_error(
            "A list change from an earlier session was not confirmed by the server. Try it again, or discard it to keep the lists as they are on the server.".into(),
        );
    }
}

fn install_downloads(ui: &PlayerWindow, inventory: &mut Vec<Download>, mut entries: Vec<Download>) {
    entries.sort_by(|a, b| {
        (&a.track.title, &a.track.artist, &a.id).cmp(&(&b.track.title, &b.track.artist, &b.id))
    });
    ui.set_download_rows(ModelRc::new(VecModel::from(
        entries
            .iter()
            .map(|entry| {
                let status = match entry.status {
                    DownloadStatus::Pending => "Queued",
                    DownloadStatus::Complete => "Downloaded",
                    DownloadStatus::Failed => "Failed",
                    DownloadStatus::Cancelled => "Cancelled",
                };
                DownloadRow {
                    id: entry.id.clone().into(),
                    title: entry.track.title.clone().into(),
                    detail: format!("{status} · {}", entry.track.artist).into(),
                    pending: entry.status == DownloadStatus::Pending,
                    complete: entry.status == DownloadStatus::Complete,
                }
            })
            .collect::<Vec<_>>(),
    )));
    *inventory = entries;
}
impl Controller {
    pub fn new(
        ui: &PlayerWindow,
        state: &Rc<RefCell<State>>,
        adapter: Arc<Adapter>,
        generation: Generation,
        open_url: crate::UrlOpener,
    ) -> anyhow::Result<Self> {
        let catalog = state.borrow().catalog.clone();
        let accepted = catalog.snapshot();
        let source_view = adapter.source_view(&accepted, None)?;
        let view = Rc::new(RefCell::new(View {
            connections: source_view.connections,
            selected: source_view.selected,
            snapshot: source_view.catalog,
            list: None,
            collection: None,
            online: accepted.source_online.unwrap_or(false),
            connection: ConnectionState::Disconnected,
            operation: None,
            list_rows: RefCell::default(),
            optimistic: None,
            recent: Vec::new(),
        }));
        let (tx, rx) = mpsc::channel();
        let download_events = tx.clone();
        let download_adapter = adapter.clone();
        let inventory_updates = std::sync::Mutex::new(());
        let download_records = Rc::new(RefCell::new(Vec::new()));
        install_downloads(ui, &mut download_records.borrow_mut(), adapter.downloads()?);
        let downloads = amp_app::downloads::Downloads::new(
            adapter.clone(),
            Arc::new(move |event| {
                match event {
                    amp_app::downloads::Event::Message(message) => {
                        let _ = download_events.send(Event::DownloadMessage(message));
                    }
                    amp_app::downloads::Event::Changed => {
                        // Batch completion and cancellation publish from different workers.
                        let _update = inventory_updates.lock().unwrap();
                        let _ =
                            download_events.send(Event::Downloads(download_adapter.downloads()));
                    }
                }
            }),
        )?;
        super::wire_source_selection(ui, state);
        {
            let mut view = view.borrow_mut();
            view.recent = view
                .selected
                .map(|key| adapter.recent_lists(key))
                .unwrap_or_default();
        }
        if view.borrow().selected.is_some() {
            install(ui, state, &view.borrow(), &download_records.borrow());
        }
        sync_banner(ui, &adapter, view.borrow().selected);
        ui.set_download_budget(
            (adapter.budget()? as f64 / (1024. * 1024. * 1024.))
                .to_string()
                .into(),
        );
        ui.set_collector_playback_format(match adapter.playback_format()? {
            PlaybackFormat::Original => 0,
            PlaybackFormat::HighBandwidth => 1,
            PlaybackFormat::LowBandwidth { bitrate_kbps } => {
                ui.set_collector_opus_bitrate(bitrate_kbps.to_string().into());
                2
            }
        });
        {
            let a = adapter.clone();
            let weak = ui.as_weak();
            let tx = tx.clone();
            let catalog = catalog.clone();
            ui.on_save_collector_playback(move || {
                let Some(ui) = weak.upgrade() else { return };
                let format = match ui.get_collector_playback_format() {
                    0 => PlaybackFormat::Original,
                    1 => PlaybackFormat::HighBandwidth,
                    2 => {
                        let Ok(bitrate_kbps) =
                            ui.get_collector_opus_bitrate().trim().parse::<u16>()
                        else {
                            ui.set_collector_playback_status(
                                "Enter an Opus bitrate from 6 to 256 kbps".into(),
                            );
                            return;
                        };
                        PlaybackFormat::LowBandwidth { bitrate_kbps }
                    }
                    _ => return,
                };
                let a = a.clone();
                let tx = tx.clone();
                catalog.persist(Box::new(move || {
                    let _ = tx.send(Event::PlaybackFormatSaved(a.set_playback_format(format)));
                    Ok(())
                }));
            });
        }

        {
            let a = adapter.clone();
            let tx = tx.clone();
            let g = generation.clone();
            let weak = ui.as_weak();
            let catalog = catalog.clone();
            let view = view.clone();
            ui.on_connect_server(move || {
                let Some(ui) = weak.upgrade() else { return };
                let url = match ui.get_server_url().parse() {
                    Ok(url) => url,
                    Err(e) => {
                        connection_error(&ui, format!("Invalid URL: {e}"));
                        return;
                    }
                };
                let trust = if ui.get_certificate_path().is_empty() {
                    None
                } else {
                    match read_trust(Path::new(ui.get_certificate_path().as_str())) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            connection_error(&ui, format!("Could not read the trust file: {e:#}"));
                            return;
                        }
                    }
                };
                ui.set_collector_busy(true);
                ui.set_connection_status("Connecting to server…".into());
                let v = g.advance();
                view.borrow_mut().operation = Some((v, catalog.snapshot().source_load_revision));
                let a = a.clone();
                let tx = tx.clone();
                let g = g.clone();
                let catalog = catalog.clone();
                std::thread::spawn(move || {
                    let result = (|| {
                        runtime()?.block_on(a.enroll(
                            url,
                            trust,
                            &|| g.current() != v,
                            &mut |url| {
                                let _ = tx.send(Event::Approval(v, url));
                            },
                        ))
                    })();
                    let result = result.and_then(|key| {
                        if g.current() != v {
                            anyhow::bail!("Catalog operation cancelled");
                        }
                        open_source(&catalog, v, key);
                        Ok(Connected {
                            connections: a.connections()?,
                        })
                    });
                    let _ = tx.send(Event::Connected(v, result.map(Box::new)));
                });
            });
        }
        {
            let g = generation.clone();
            let weak = ui.as_weak();
            let catalog = catalog.clone();
            let view = view.clone();
            let state = state.clone();
            ui.on_cancel_connect(move || {
                let v = g.advance();
                view.borrow_mut().operation = None;
                if let Some(key) = view.borrow().selected {
                    open_source(&catalog, v, key);
                } else if state.borrow().library.id == "demo" {
                    catalog.show_transient((*state.borrow().library).clone());
                } else {
                    catalog.restore_local();
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_collector_busy(false);
                    ui.set_connection_status("Operation cancelled".into());
                }
            });
        }
        {
            let g = generation.clone();
            let view = view.clone();
            let weak = ui.as_weak();
            let catalog = catalog.clone();
            ui.on_refresh_server(move || {
                if let (Some(key), Some(ui)) = (view.borrow().selected, weak.upgrade()) {
                    let v = g.advance();
                    ui.set_collector_busy(true);
                    open_source(&catalog, v, key);
                }
            });
        }
        {
            let a = adapter.clone();
            let view = view.clone();
            let tx = tx.clone();
            let weak = ui.as_weak();
            let source_catalog = catalog.clone();
            ui.on_remove_server(move || {
                let key = view.borrow().selected;
                if let (Some(key), Some(ui)) = (key, weak.upgrade()) {
                    if ui.get_removing_connection() {
                        return;
                    }
                    ui.set_removing_connection(true);
                    ui.set_connection_status("Removing connection…".into());
                    let a = a.clone();
                    let tx = tx.clone();
                    let source_catalog = source_catalog.clone();
                    // Keychain can wait for OS authorization. Keep that wait,
                    // and its failure, off the UI thread.
                    std::thread::spawn(move || {
                        let result = a.remove(key);
                        if result.is_ok() {
                            source_catalog.refresh_sources();
                        }
                        let _ = tx.send(Event::Removed(key, result));
                    });
                }
            });
        }
        {
            let state = state.clone();
            let weak = ui.as_weak();
            ui.on_album_source_selected(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let mut s = state.borrow_mut();
                let Some(index) = s.selected else { return };
                let album = &s.library.albums[index];
                let key = album_key(album);
                let sources = disc_sources(album);
                if i == 0 {
                    s.selected_sources.remove(&key);
                } else if let Some((source, _, _)) = sources.get(i as usize - 1) {
                    s.selected_sources.insert(key, source.clone());
                } else {
                    return;
                }
                browse::clear_track_context(&mut s);
                select(&ui, &mut s, index);
            });
        }
        {
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            let inventory = download_records.clone();
            ui.on_collection_changed(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let mut view = view.borrow_mut();
                view.collection = if i > 0 {
                    view.snapshot
                        .as_ref()
                        .and_then(|s| s.collections.get(i as usize - 1))
                        .map(|c| c.id)
                } else {
                    None
                };
                view.list = None;
                install(&ui, &state, &view, &inventory.borrow());
            });
        }
        {
            let view = view.clone();
            let weak = ui.as_weak();
            let inventory = download_records.clone();
            ui.on_open_list_at(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let mut view = view.borrow_mut();
                let list = usize::try_from(i)
                    .ok()
                    .and_then(|index| Some(view.snapshot.as_ref()?.lists.get(index)?.list.id));
                if view.list == list {
                    return;
                }
                view.list = list;
                ui.set_collector_list_loading(
                    view.online
                        && view.selected_list().is_some_and(|list| {
                            list.entries.items.len() < list.list.entry_count as usize
                        }),
                );
                render_list(&ui, &view, &inventory.borrow());
            });
        }
        {
            let inventory = download_records.clone();
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            ui.on_play_list_from(move |index| {
                let Some(ui) = weak.upgrade() else { return };
                let view = view.borrow();
                let Some(key) = view.selected else { return };
                let Some(list) = view.selected_list() else {
                    return;
                };
                let Ok(index) = usize::try_from(index) else {
                    return;
                };
                let inventory = inventory.borrow();
                let tracks = PlaylistTracks::new(key, list, &inventory);
                let playable = |t: &Track| playlist_playable(t, view.online);
                let Some(first) = tracks.get(index).filter(playable) else {
                    ui.set_connection_status("This entry is not playable".into());
                    return;
                };
                let tracks = std::iter::once(first)
                    .chain(tracks.tracks_from(index + 1).filter(playable))
                    .collect();
                state.borrow().playback.play_list(tracks);
                ui.set_connection_status("Playing list from track".into());
            });
        }
        {
            let inventory = download_records.clone();
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            ui.on_play_list(move |shuffle| {
                let Some(ui) = weak.upgrade() else { return };
                let view = view.borrow();
                let Some(key) = view.selected else { return };
                let Some(list) = view.selected_list() else {
                    return;
                };
                let inventory = inventory.borrow();
                let mut tracks = PlaylistTracks::new(key, list, &inventory)
                    .tracks_from(0)
                    .filter(|t| playlist_playable(t, view.online))
                    .collect::<Vec<_>>();
                let excluded = list.entries.items.len() - tracks.len();
                if shuffle {
                    use rand::seq::SliceRandom;
                    tracks.shuffle(&mut rand::rng());
                }
                if tracks.is_empty() {
                    ui.set_connection_status("No playable tracks in this list".into());
                    return;
                }
                state.borrow().playback.play_list(tracks);
                ui.set_connection_status(
                    format!("Playing list · {excluded} unavailable entries excluded").into(),
                );
            });
        }
        let start: Rc<StartJob> = {
            let a = adapter.clone();
            let view = view.clone();
            let tx = tx.clone();
            let g = generation.clone();
            let catalog = catalog.clone();
            Rc::new(move |ui, key, job, purpose| {
                let v = g.current();
                view.borrow_mut().operation = Some((v, catalog.snapshot().source_load_revision));
                ui.set_collector_busy(true);
                let a = a.clone();
                let tx = tx.clone();
                let catalog = catalog.clone();
                std::thread::spawn(move || {
                    let applied = match runtime() {
                        Ok(rt) => rt.block_on(run_job(&a, key, job)),
                        Err(error) => Applied {
                            outcomes: Vec::new(),
                            error: Some(error),
                        },
                    };
                    let resync = applied.error.is_none() && needs_resync(&applied.outcomes);
                    let _ = tx.send(Event::Mutation(v, key, purpose, applied));
                    // Replayed or dropped commands have unknown effects; reread the server.
                    if resync {
                        open_source(&catalog, v, key);
                    }
                });
            })
        };
        {
            let view = view.clone();
            let weak = ui.as_weak();
            let start = start.clone();
            ui.on_list_command(move |action, entry, name| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.get_collector_busy() {
                    return;
                }
                let request = playlist_request(&view.borrow(), action, entry, &name);
                let (key, request) = match request {
                    Ok(request) => request,
                    Err(error) => {
                        ui.set_status(error.to_string().into());
                        return;
                    }
                };
                // Show removals and moves at once; the server's answer settles the revision.
                if let PlaylistAction::Update {
                    list,
                    change:
                        PlaylistChange::Edit(
                            change @ (api::CollectionListChange::Remove { .. }
                            | api::CollectionListChange::Move { .. }),
                        ),
                } = &request
                    && let Ok(i) = usize::try_from(entry)
                {
                    let mut current = view.borrow_mut();
                    if let Some(row) = current.list_mut(list.id) {
                        let before = row.clone();
                        apply_list_change(row, change);
                        row.list.entry_count = row.entries.items.len() as u32;
                        current.optimistic = Some(before);
                    }
                    if let Some(rows) = current.list_rows.borrow().as_ref() {
                        match action {
                            ListAction::RemoveEntry => rows.remove(i),
                            ListAction::MoveUp => rows.move_row(i, i - 1),
                            _ => rows.move_row(i, i + 1),
                        }
                    }
                    let last = ui.get_list_rows().row_count() as i32 - 1;
                    let i = i as i32;
                    ui.set_list_entry(match action {
                        ListAction::RemoveEntry => i.min(last),
                        ListAction::MoveUp => i - 1,
                        _ => i + 1,
                    });
                    render_lists(&ui, &current);
                }
                start(&ui, key, Job::Single(request), Purpose::Edit);
            });
        }
        let picker = Rc::new(RefCell::new(AddPicker::default()));
        {
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            let picker = picker.clone();
            ui.on_add_to_list(move |source, row| {
                let Some(ui) = weak.upgrade() else { return };
                let view = view.borrow();
                let Some(snapshot) = view.snapshot.as_ref() else {
                    return;
                };
                let Some((target, title, collection)) =
                    add_target(&ui, &state.borrow(), &view, source, row).and_then(
                        |(target, title)| {
                            let collection = target.collection()?;
                            Some((target, title, collection))
                        },
                    )
                else {
                    ui.set_status("This can't be added to a list".into());
                    return;
                };
                let lists = by_recent_use(
                    snapshot
                        .lists
                        .iter()
                        .filter(|l| l.list.can_edit && l.list.collection_id == collection),
                    &view.recent,
                );
                ui.set_add_to_list_rows(list_cards(snapshot, lists.iter().copied()));
                ui.set_add_to_list_can_create(
                    snapshot
                        .collections
                        .iter()
                        .any(|c| c.id == collection && c.can_edit_playlists),
                );
                ui.set_add_to_list_disc(matches!(&target, AddTarget::Album { disc: Some(_), .. }));
                ui.set_add_to_list_title(title.clone().into());
                *picker.borrow_mut() = AddPicker {
                    lists: lists.iter().map(|l| l.list.id).collect(),
                    target: Some((target, title)),
                };
                ui.set_add_to_list_open(true);
            });
        }
        // Start adding music once its list is known, or say why it can't be.
        let add: Rc<StartAdd> = {
            let view = view.clone();
            let start = start.clone();
            Rc::new(move |ui, target, title, list, disc_only| {
                if ui.get_collector_busy() {
                    return;
                }
                let (key, online) = {
                    let view = view.borrow();
                    (view.selected, view.online)
                };
                let Some(key) = key else { return };
                if !online {
                    ui.set_status("Adding to a list needs a connection to the server".into());
                    return;
                }
                if let AddTo::Existing(list) = &list
                    && target.collection() != Some(list.collection_id)
                {
                    ui.set_status(
                        format!("{title} is in a different collection from “{}”", list.name).into(),
                    );
                    return;
                }
                let changes = target.changes(disc_only);
                start(ui, key, Job::Add { list, changes }, Purpose::Add(title));
            })
        };
        {
            let view = view.clone();
            let weak = ui.as_weak();
            let picker = picker.clone();
            let add = add.clone();
            ui.on_add_to_list_choose(move |index| {
                let Some(ui) = weak.upgrade() else { return };
                let chosen = {
                    let picker = picker.borrow();
                    let view = view.borrow();
                    (|| {
                        let (target, title) = picker.target.clone()?;
                        let id = picker.lists.get(usize::try_from(index).ok()?)?;
                        let list = view
                            .snapshot
                            .as_ref()?
                            .lists
                            .iter()
                            .find(|l| l.list.id == *id)?;
                        Some((target, title, list.list.clone()))
                    })()
                };
                let Some((target, title, list)) = chosen else {
                    return;
                };
                let disc_only = ui.get_add_to_list_disc_only();
                add(&ui, target, title, AddTo::Existing(list), disc_only);
            });
        }
        {
            let weak = ui.as_weak();
            let picker = picker.clone();
            let add = add.clone();
            ui.on_add_to_list_create(move |name| {
                let Some(ui) = weak.upgrade() else { return };
                let Some((target, title)) = picker.borrow().target.clone() else {
                    return;
                };
                let Some(collection) = target.collection() else {
                    return;
                };
                let list = AddTo::New {
                    collection,
                    name: name.to_string(),
                };
                let disc_only = ui.get_add_to_list_disc_only();
                add(&ui, target, title, list, disc_only);
            });
        }
        {
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            let add = add.clone();
            ui.on_add_to_recent_list(move |source, row, index| {
                let Some(ui) = weak.upgrade() else { return };
                let chosen = {
                    let view = view.borrow();
                    let list = usize::try_from(index)
                        .ok()
                        .and_then(|i| menu_lists(&view).get(i).map(|l| l.list.clone()));
                    list.zip(add_target(&ui, &state.borrow(), &view, source, row))
                };
                let Some((list, (target, title))) = chosen else {
                    ui.set_status("This can't be added to a list".into());
                    return;
                };
                add(&ui, target, title, AddTo::Existing(list), false);
            });
        }
        {
            let a = adapter.clone();
            let weak = ui.as_weak();
            let tx = tx.clone();
            let catalog = catalog.clone();
            ui.on_save_budget(move || {
                let Some(ui) = weak.upgrade() else { return };
                let bytes = (|| -> anyhow::Result<u64> {
                    let gib = ui.get_download_budget().parse::<f64>()?;
                    if !gib.is_finite() || gib <= 0.0 || gib > 1048576.0 {
                        anyhow::bail!("Enter a positive GiB budget");
                    }
                    Ok((gib * 1024.0 * 1024.0 * 1024.0) as u64)
                })();
                match bytes {
                    Err(error) => ui.set_download_status(error.to_string().into()),
                    Ok(bytes) => {
                        let a = a.clone();
                        let tx = tx.clone();
                        catalog.persist(Box::new(move || {
                            let result = a.set_budget(bytes);
                            let message = match &result {
                                Ok(()) => "Storage limit saved.".into(),
                                Err(error) => error.to_string(),
                            };
                            let _ = tx.send(Event::DownloadMessage(message));
                            result
                        }));
                    }
                }
            });
        }
        {
            let state = state.clone();
            let view = view.clone();
            let weak = ui.as_weak();
            let downloads = downloads.clone();
            let inventory = download_records.clone();
            ui.on_download_action(move |verb| {
                let Some(ui) = weak.upgrade() else { return };
                if verb == 5 {
                    if let Err(error) = downloads.request_cancel() {
                        ui.set_download_status(error.to_string().into());
                    }
                    return;
                }
                let tracks = (|| -> anyhow::Result<Vec<Track>> {
                    if verb == 4 {
                        let tracks: Vec<_> = inventory
                            .borrow()
                            .iter()
                            .filter(|d| d.status != DownloadStatus::Complete)
                            .map(|d| d.track.clone())
                            .collect();
                        anyhow::ensure!(!tracks.is_empty(), "No downloads to retry.");
                        return Ok(tracks);
                    }
                    let view = view.borrow();
                    if verb == 2 || verb == 3 {
                        let key = view.selected.context("Select server")?;
                        let list = view.selected_list().context("Select list")?;
                        Ok(PlaylistTracks::new(key, list, &inventory.borrow())
                            .tracks_from(0)
                            .collect())
                    } else {
                        let s = state.borrow();
                        let index = s.selected.context("Select album")?;
                        let tracks = displayed_tracks(&s, index);
                        if verb == 6 {
                            Ok(vec![
                                tracks
                                    .get(ui.get_selected_track() as usize)
                                    .context("Select track")?
                                    .clone(),
                            ])
                        } else {
                            Ok(tracks)
                        }
                    }
                })();
                let tracks = match tracks {
                    Ok(t) => t,
                    Err(e) => {
                        ui.set_download_status(e.to_string().into());
                        if verb != 4 {
                            ui.set_status(e.to_string().into());
                        }
                        return;
                    }
                };
                let action = if verb == 1 || verb == 3 {
                    amp_app::downloads::Action::Remove
                } else {
                    amp_app::downloads::Action::Download
                };
                if let Err(error) = downloads.start(tracks, action) {
                    ui.set_download_status(error.to_string().into());
                    ui.set_status(error.to_string().into());
                }
            });
        }
        {
            let inventory = download_records.clone();
            let weak = ui.as_weak();
            ui.on_remove_download_record(move |id| {
                let Some(ui) = weak.upgrade() else { return };
                let track = inventory
                    .borrow()
                    .iter()
                    .find(|entry| id == entry.id && entry.status != DownloadStatus::Pending)
                    .map(|entry| entry.track.clone());
                if let Some(track) = track
                    && let Err(error) =
                        downloads.start(vec![track], amp_app::downloads::Action::RemoveRecord)
                {
                    ui.set_download_status(error.to_string().into());
                }
            });
        }
        Ok(Self {
            rx,
            tx,
            ui: ui.as_weak(),
            state: state.clone(),
            adapter,
            view,
            generation,
            connection_probe: None,
            open_url,
            source_revision: (accepted.generation, accepted.library_revision),
            source_projection: None,
            album_request: SelectionRead::default(),
            list_request: SelectionRead::default(),
            download_records,
        })
    }
    fn update_source_view(&mut self, ui: &PlayerWindow, catalog: &amp_app::catalog::Snapshot) {
        let current = self.generation.current();
        if let Some(pending) = &self.source_projection {
            match pending.try_recv() {
                Ok(result) => {
                    self.source_projection = None;
                    if result.generation == current
                        && result.generation == catalog.generation
                        && result.source == catalog.source
                        && result.revision <= catalog.library_revision
                    {
                        match result.view {
                            Ok(source) => self.install_source_view(
                                ui,
                                source,
                                catalog.source_online.unwrap_or(false),
                            ),
                            Err(error) => ui.set_connection_status(
                                format!("Could not read Collector library: {error}").into(),
                            ),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.source_projection = None,
            }
        }

        let revision = (catalog.generation, catalog.library_revision);
        if catalog.generation != current || self.source_revision == revision {
            return;
        }
        let collector = matches!(&catalog.source, CatalogSource::External(id)
            if catalog.sources.iter().any(|s| &s.id == id && s.provider == "collector"));
        {
            let mut view = self.view.borrow_mut();
            if let Some(key) = view.selected
                && catalog.source != CatalogSource::External(amp_collector::source_id(key))
            {
                view.selected = None;
                view.collection = None;
                view.list = None;
                view.snapshot = None;
                view.online = false;
                view.connection = ConnectionState::Disconnected;
                ui.set_collector_active(collector);
                ui.set_collector_online(false);
                ui.set_connection_kind(ConnectionKind::Disconnected);
                render_list(ui, &view, &self.download_records.borrow());
            }
        }
        if !collector {
            self.source_revision = revision;
            return;
        }
        if self.source_projection.is_some() {
            return;
        }
        self.source_revision = revision;
        let adapter = self.adapter.clone();
        let snapshot = catalog.clone();
        let collection = self.view.borrow().collection;
        let (tx, rx) = mpsc::channel();
        self.source_projection = Some(rx);
        std::thread::spawn(move || {
            let view = adapter.source_view(&snapshot, collection);
            let _ = tx.send(SourceProjection {
                generation: snapshot.generation,
                revision: snapshot.library_revision,
                source: snapshot.source,
                view,
            });
        });
    }

    fn install_source_view(&self, ui: &PlayerWindow, source: SourceView, online: bool) {
        let mut view = self.view.borrow_mut();
        if view.selected != source.selected {
            view.collection = None;
            view.list = None;
            view.snapshot = None;
            view.connection = ConnectionState::Disconnected;
        }
        let entry_id = view
            .selected_list()
            .and_then(|list| list.entries.items.get(ui.get_list_entry() as usize))
            .map(|entry| entry.id);
        let mut next = source.catalog;
        // A projection can be older than edits already applied here: keep
        // newer revisions, loaded entries, and an edit still awaiting the server.
        if let Some(snapshot) = &mut next
            && let Some(current) = view.snapshot.as_ref()
        {
            let pending = view.optimistic.as_ref().map(|list| list.list.id);
            for incoming in &mut snapshot.lists {
                if let Some(kept) = current.lists.iter().find(|row| {
                    row.list.id == incoming.list.id
                        && (pending == Some(row.list.id)
                            || incoming.list.revision.0 < row.list.revision.0
                            || (incoming.list.revision == row.list.revision
                                && incoming.entries.items.len() < row.entries.items.len()))
                }) {
                    *incoming = kept.clone();
                }
            }
        }
        view.connections = source.connections;
        if view.selected != source.selected {
            view.recent = source
                .selected
                .map(|key| self.adapter.recent_lists(key))
                .unwrap_or_default();
        }
        view.selected = source.selected;
        view.snapshot = next;
        if view.selected_list().is_none() {
            view.list = None;
        }
        view.online = online;
        ui.set_collector_active(view.selected.is_some());
        if view.selected.is_some() {
            install(ui, &self.state, &view, &self.download_records.borrow());
            if let Some(entry) = entry_id {
                ui.set_list_entry(
                    view.selected_list()
                        .and_then(|list| {
                            list.entries.items.iter().position(|item| item.id == entry)
                        })
                        .map_or(-1, |i| i as i32),
                );
            }
        }
    }

    /// Settle a list change: keep what the server accepted and roll back the rest.
    fn finish_mutation(
        &self,
        ui: &PlayerWindow,
        key: ConnectionKey,
        purpose: Purpose,
        applied: Applied,
    ) {
        let mut view = self.view.borrow_mut();
        if applied.error.is_some() || !needs_resync(&applied.outcomes) {
            view.operation = None;
        }
        let optimistic = view.optimistic.take();
        view.recent = self.adapter.recent_lists(key);
        // Rows already show an optimistic edit; anything else needs the rows rebuilt.
        let mut rebuild = false;
        let mut message = None;
        for outcome in &applied.outcomes {
            match outcome {
                PlaylistOutcome::Updated {
                    list,
                    change:
                        api::CollectionListChange::Remove { .. }
                        | api::CollectionListChange::Move { .. },
                } if optimistic.as_ref().is_some_and(|o| o.list.id == list.id) => {
                    if let Some(row) = view.list_mut(list.id)
                        && row.list.revision.0 < list.revision.0
                    {
                        row.list = list.clone();
                    }
                }
                outcome => {
                    if let Some(snapshot) = view.snapshot.as_mut() {
                        patch_lists(&mut snapshot.lists, outcome);
                    }
                    rebuild = true;
                }
            }
            message = match outcome {
                PlaylistOutcome::Created(list) => {
                    if matches!(purpose, Purpose::Edit) {
                        view.list = Some(list.id);
                        ui.set_collector_list_loading(false);
                    }
                    Some(format!("Created “{}”", list.name))
                }
                PlaylistOutcome::Updated { list, change } => match change {
                    api::CollectionListChange::Delete => {
                        if view.list == Some(list.id) {
                            view.list = None;
                        }
                        Some(format!("Deleted “{}”", list.name))
                    }
                    api::CollectionListChange::Rename { .. } => {
                        Some(format!("Renamed to “{}”", list.name))
                    }
                    api::CollectionListChange::Append { .. } => Some(match &purpose {
                        Purpose::Add(what) => format!("Added {what} to “{}”", list.name),
                        Purpose::Edit => format!("Added to “{}”", list.name),
                    }),
                    api::CollectionListChange::Remove { .. } => {
                        Some(format!("Removed from “{}”", list.name))
                    }
                    api::CollectionListChange::Move { .. } => message,
                },
                PlaylistOutcome::Retried => Some("Saved the list change".into()),
                PlaylistOutcome::Discarded => Some("Discarded the list change".into()),
            };
        }
        if let Some(error) = &applied.error {
            if let Some(before) = optimistic
                && let Some(row) = view.list_mut(before.list.id)
                && row.list.revision == before.list.revision
            {
                *row = before;
                rebuild = true;
            }
            let pending = self
                .adapter
                .pending_commands(key)
                .is_ok_and(|pending| !pending.is_empty());
            ui.set_list_sync_pending(pending);
            ui.set_list_sync_error(
                if pending {
                    format!(
                        "The last list change wasn't saved: {error:#}. Try it again, or discard it to keep the list as it is on the server."
                    )
                } else {
                    format!("Couldn't change the list: {error:#}")
                }
                .into(),
            );
            message = Some(format!("Couldn't change the list: {error:#}"));
        } else {
            ui.set_list_sync_error("".into());
            ui.set_list_sync_pending(false);
            if matches!(purpose, Purpose::Add(_)) {
                ui.set_add_to_list_open(false);
            }
        }
        if rebuild {
            let selected = view
                .selected_list()
                .and_then(|list| list.entries.items.get(ui.get_list_entry() as usize))
                .map(|entry| entry.id);
            render_list(ui, &view, &self.download_records.borrow());
            if let Some(entry) = selected {
                ui.set_list_entry(
                    view.selected_list()
                        .and_then(|list| list.entries.items.iter().position(|e| e.id == entry))
                        .map_or(-1, |i| i as i32),
                );
            }
        } else {
            render_lists(ui, &view);
        }
        if let Some(message) = message {
            ui.set_status(message.into());
        }
    }

    pub fn poll(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let catalog = self.state.borrow().catalog.snapshot();
        self.update_source_view(&ui, &catalog);
        let selected = {
            let state = self.state.borrow();
            if ui.get_show_library()
                || ui.get_show_queue()
                || ui.get_lists_open()
                || ui.get_artist_view()
                || ui.get_home()
                || ui.get_settings()
            {
                None
            } else {
                state
                    .selected
                    .and_then(|index| state.library.albums.get(index))
                    .map(|a| a.id.clone())
            }
        };
        let key = self.view.borrow().selected.filter(|key| {
            catalog.generation == self.generation.current()
                && catalog.source == CatalogSource::External(amp_collector::source_id(*key))
        });
        let album = key
            .zip(selected)
            .map(|(key, id)| (catalog.generation, key, id));
        if !self.album_request.keep_target(album.as_ref()) {
            ui.set_collector_album_loading(false);
        }
        if catalog.source_online == Some(true)
            && let Some((v, key, id)) = album
            && let Some(ticket) = self.album_request.begin((v, key, id.clone()))
        {
            ui.set_collector_album_loading(true);
            ui.set_status("Loading album tracks…".into());
            let adapter = self.adapter.clone();
            let generation = self.generation.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = (|| {
                    runtime()?.block_on(adapter.load_album(key, id.item.parse()?, &|| {
                        generation.current() != v || ticket.is_cancelled()
                    }))
                })();
                let _ = tx.send(Event::AlbumLoaded(
                    v,
                    key,
                    id,
                    ticket.serial,
                    result.map(Box::new),
                ));
            });
        }
        let list = self
            .view
            .borrow()
            .selected_list()
            .filter(|_| ui.get_lists_open() && self.view.borrow().optimistic.is_none())
            .map(|l| {
                (
                    l.list.id,
                    l.entries.items.len(),
                    l.list.entry_count as usize,
                )
            });
        let target = key
            .zip(list)
            .map(|(key, (id, _, _))| (catalog.generation, key, id));
        if !self.list_request.keep_target(target.as_ref()) {
            ui.set_collector_list_loading(false);
        }
        if !catalog.scanning
            && catalog.source_online == Some(true)
            && let Some((_, loaded, total)) = list
            && loaded < total
            && let Some((v, key, id)) = target
            && let Some(ticket) = self.list_request.begin((v, key, id))
        {
            ui.set_collector_list_loading(true);
            let adapter = self.adapter.clone();
            let generation = self.generation.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = (|| {
                    runtime()?.block_on(adapter.load_list(key, id, &|| {
                        generation.current() != v || ticket.is_cancelled()
                    }))
                })();
                let _ = tx.send(Event::ListLoaded(
                    v,
                    key,
                    id,
                    ticket.serial,
                    result.map(Box::new),
                ));
            });
        }

        while let Ok(event) = self.rx.try_recv() {
            let current = self.generation.current();
            match event {
                Event::Approval(v, url) if v == current => {
                    ui.set_connection_status(
                        format!("Approve Trash Amp in your browser: {url}").into(),
                    );
                    let tx = self.tx.clone();
                    (self.open_url)(
                        &url.clone(),
                        Box::new(move |result| {
                            if let Err(error) = result {
                                let _ = tx.send(Event::OpenFailed(v, format!(
                                "Could not open the approval page: {error}\n\nOpen this address in your browser to approve Trash Amp, or cancel and retry:\n{url}"
                            )));
                            }
                        }),
                    );
                }
                Event::OpenFailed(v, message) if v == current => connection_error(&ui, message),
                Event::Connected(v, result) if v == current => match result {
                    Ok(ready) => {
                        let mut view = self.view.borrow_mut();
                        view.connections = ready.connections;
                        ui.set_connection_status("Connected; loading library…".into());
                    }
                    Err(e) => {
                        self.view.borrow_mut().operation = None;
                        connection_error(&ui, format!("Could not connect to Collector: {e:#}"))
                    }
                },
                Event::Mutation(v, key, purpose, applied)
                    if v == current && self.view.borrow().selected == Some(key) =>
                {
                    self.finish_mutation(&ui, key, purpose, applied);
                }
                Event::Removed(key, result) => {
                    ui.set_removing_connection(false);
                    match result {
                        Ok(()) => {
                            let mut view = self.view.borrow_mut();
                            if let Some(connection) =
                                view.connections.iter_mut().find(|c| c.key == key)
                            {
                                connection.removed = true;
                            }
                            if view.selected == Some(key) {
                                view.online = false;
                                view.connection = ConnectionState::Disconnected;
                                ui.set_collector_online(false);
                                ui.set_connection_kind(ConnectionKind::Disconnected);
                                ui.set_connection_status(
                                    "Connection removed; completed downloads retained".into(),
                                );
                            }
                        }
                        Err(error) => ui.set_connection_status(
                            format!("Could not remove connection: {error}").into(),
                        ),
                    }
                }
                Event::DownloadMessage(message) => {
                    ui.set_download_status(message.clone().into());
                    ui.set_status(message.into());
                }
                Event::ConnectionChecked(v, key, status)
                    if v == current
                        && self.view.borrow().selected == Some(key)
                        && self
                            .view
                            .borrow()
                            .connections
                            .iter()
                            .any(|c| c.key == key && !c.removed) =>
                {
                    if let Some((probe_v, probe_key, _, pending)) = &mut self.connection_probe
                        && *probe_v == v
                        && *probe_key == key
                    {
                        *pending = false;
                    }
                    ui.set_connection_kind(connection_kind(status));
                    let online = status != ConnectionState::Disconnected;
                    let mut view = self.view.borrow_mut();
                    let recovered = !view.online && online;
                    view.online = online;
                    view.connection = status;
                    if recovered
                        && catalog.generation == current
                        && catalog.requested_source.is_none()
                        && !catalog.scanning
                        && catalog.source == CatalogSource::External(amp_collector::source_id(key))
                    {
                        self.state.borrow().catalog.revalidate_source();
                    }
                    ui.set_collector_online(online);
                }
                Event::AlbumLoaded(v, key, id, serial, result)
                    if self.album_request.accepts(&(v, key, id.clone()), serial)
                        && v == current
                        && self.view.borrow().selected == Some(key) =>
                {
                    ui.set_collector_album_loading(false);
                    match result {
                        Ok(album) => self.state.borrow().catalog.loaded_external_album(
                            v,
                            amp_collector::source_id(key),
                            *album,
                        ),
                        Err(error) => {
                            ui.set_status(format!("Could not load album tracks: {error}").into())
                        }
                    }
                }
                Event::ListLoaded(v, key, id, serial, result)
                    if self.list_request.accepts(&(v, key, id), serial)
                        && v == current
                        && ui.get_lists_open()
                        && {
                            let view = self.view.borrow();
                            view.selected == Some(key) && view.list == Some(id)
                        } =>
                {
                    ui.set_collector_list_loading(false);
                    match result {
                        Err(error) => {
                            ui.set_status(format!("Could not load playlist: {error}").into())
                        }
                        Ok(loaded) => {
                            let mut view = self.view.borrow_mut();
                            if let Some(row) = view.snapshot.as_mut().and_then(|snapshot| {
                                snapshot.lists.iter_mut().find(|row| row.list.id == id)
                            }) {
                                if loaded.list.revision.0 >= row.list.revision.0 {
                                    *row = *loaded;
                                    render_list(&ui, &view, &self.download_records.borrow());
                                } else {
                                    self.list_request.cancel();
                                }
                            }
                        }
                    }
                }
                Event::PlaybackFormatSaved(result) => {
                    ui.set_collector_playback_status(match result {
                        Ok(()) => "Saved. Applies when playback opens a track.".into(),
                        Err(error) => error.to_string().into(),
                    });
                }
                Event::Downloads(result) => {
                    match result {
                        Ok(entries) => {
                            install_downloads(&ui, &mut self.download_records.borrow_mut(), entries)
                        }
                        Err(error) => ui.set_download_status(
                            format!("Could not read downloads: {error}").into(),
                        ),
                    }
                    if self.view.borrow().selected.is_some() && ui.get_collector_active() {
                        // Adapter completion already notified the source owner.
                        let entry = ui.get_list_entry();
                        render_list(&ui, &self.view.borrow(), &self.download_records.borrow());
                        ui.set_list_entry(entry);
                    }
                }
                // A change for a source no longer shown: drop its pending edit
                // so list loading resumes.
                Event::Mutation(..) => self.view.borrow_mut().optimistic = None,
                _ => {}
            }
        }
        let current = self.generation.current();
        {
            let mut view = self.view.borrow_mut();
            if view.operation.is_some_and(|(generation, revision)| {
                generation != current
                    || (catalog.generation == current
                        && !catalog.scanning
                        && catalog.source_load_revision > revision)
            }) {
                view.operation = None;
            }
        }
        let operation_busy = self
            .view
            .borrow()
            .operation
            .is_some_and(|(generation, _)| generation == current);
        let source_busy = catalog.generation == current
            && catalog.scanning
            && catalog.requested_source.as_ref().map_or(
                self.view.borrow().selected.is_some(),
                |id| {
                    catalog
                        .sources
                        .iter()
                        .any(|s| &s.id == id && s.provider == "collector")
                },
            );
        ui.set_collector_busy(operation_busy || source_busy);
        // Recheck after a source change and periodically so a network change is
        // reflected without requiring playback or a library refresh.
        let view = self.view.borrow();
        if !ui.get_collector_busy()
            && !ui.get_removing_connection()
            && let Some(key) = view.selected
            && view.connections.iter().any(|c| c.key == key && !c.removed)
        {
            let v = self.generation.current();
            if self
                .connection_probe
                .is_none_or(|(old_v, old_key, started, pending)| {
                    old_v != v
                        || old_key != key
                        || (!pending && started.elapsed() >= Duration::from_secs(30))
                })
            {
                self.connection_probe = Some((v, key, std::time::Instant::now(), true));
                let adapter = self.adapter.clone();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let status = adapter.connection_state(key);
                    let _ = tx.send(Event::ConnectionChecked(v, key, status));
                });
            }
        }
    }
}
fn read_trust(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        anyhow::bail!("Trust file exceeds 64 KiB")
    };
    Ok(String::from_utf8(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(services: &TestServices, library: Library) -> Rc<RefCell<State>> {
        Rc::new(RefCell::new(State {
            catalog: services.catalog(),
            folders: services.catalog().snapshot().folders,
            catalog_revision: 0,
            catalog_replacement: 0,
            library: Arc::new(library),
            playback: services.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: Preferences::default(),
        }))
    }
    #[test]
    fn lazy_album_install_restores_search_track_source_and_scroll_without_reviving_back_navigation()
    {
        i_slint_backend_testing::init_no_event_loop();
        let data = std::env::temp_dir().join(format!("trash-lazy-search-{}", uuid::Uuid::new_v4()));
        let services = TestServices::new(&data.join("library.sqlite3"));
        let mut full = amp_library::demo_library();
        full.albums.truncate(1);
        full.albums[0].id.provider = "collector".into();
        let mut alternate = full.albums[0].tracks.clone();
        for track in &mut alternate {
            track.id.item.push_str("-alternate");
            track.title = format!("Alternate {}", track.title);
        }
        full.albums[0].tracks.extend(alternate);
        for (i, track) in full.albums[0].tracks.iter_mut().enumerate() {
            track.source = Some(amp_core::SourceCoordinates {
                connection: "c".into(),
                collection: "c".into(),
                item: "i".into(),
                capture: "c".into(),
                representation: if i < 10 { "A" } else { "B" }.into(),
                track: (i % 10 + 1) as u16,
            });
        }
        let state = test_state(&services, full.clone());
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(980., 460.));
        ui.show().unwrap();
        browse::wire(&ui, &state);
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "library": full, "collections": [], "lists": [], "artwork": {}, "album_collections": {}, "updated": 0
        })).unwrap();
        let mut view = View {
            connections: vec![],
            selected: None,
            snapshot: Some(snapshot.into()),
            list: None,
            collection: None,
            online: true,
            connection: ConnectionState::Connected,
            operation: None,
            list_rows: RefCell::default(),
            optimistic: None,
            recent: Vec::new(),
        };
        install(&ui, &state, &view, &[]);
        ui.set_home(false);
        ui.set_search_text("Alternate Movement 8".into());
        ui.invoke_search_changed();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let row = loop {
            browse::poll(&ui, &mut state.borrow_mut());
            if let Some(row) = ui
                .get_browser_rows()
                .iter()
                .position(|r| r.kind == 3 && r.title == "Alternate Movement 8")
            {
                break row;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        // Retained/online search results can precede the album's actual tracks.
        Arc::make_mut(&mut view.snapshot.as_mut().unwrap().library).albums[0]
            .tracks
            .clear();
        install(&ui, &state, &view, &[]);
        ui.invoke_browse_action(false, row as i32, 0);
        assert_eq!(ui.get_selected_track(), -1);
        assert_eq!(ui.get_tracks().row_count(), 0);
        assert!(!ui.get_album_matching_available());
        view.snapshot.as_mut().unwrap().library = Arc::new(full.clone());
        install(&ui, &state, &view, &[]);
        assert_eq!(ui.get_album_source_index(), 2);
        assert_eq!(ui.get_tracks().row_count(), 10);
        assert_eq!(ui.get_selected_track(), 7);
        assert!(
            ui.get_track_y() < 0.,
            "The search-selected song must be scrolled into view"
        );
        assert_eq!(
            displayed_tracks(&state.borrow(), 0)[7].title,
            "Alternate Movement 8"
        );
        install(&ui, &state, &view, &[]);
        assert_eq!(
            ui.get_selected_track(),
            7,
            "Later artwork/catalog updates must preserve the restored song"
        );
        assert!(ui.get_track_y() < 0.);
        ui.set_selected_track(0);
        ui.invoke_album_track_selected();
        install(&ui, &state, &view, &[]);
        assert_eq!(
            ui.get_selected_track(),
            0,
            "A later user selection must not be replaced by the old search target"
        );

        // A second cold open is abandoned before its completion arrives.
        ui.invoke_back();
        Arc::make_mut(&mut view.snapshot.as_mut().unwrap().library).albums[0]
            .tracks
            .clear();
        install(&ui, &state, &view, &[]);
        ui.invoke_browse_action(false, row as i32, 0);
        assert_eq!(ui.get_tracks().row_count(), 0);
        ui.invoke_back();
        view.snapshot.as_mut().unwrap().library = Arc::new(full);
        install(&ui, &state, &view, &[]);
        assert!(ui.get_show_library(), "Late hydration must not undo Back");
        assert_eq!(ui.get_selected_track(), -1);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn disc_choice_survives_navigation_without_removing_searchable_tracks() {
        use slint::Model;
        i_slint_backend_testing::init_no_event_loop();
        let data = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let services = TestServices::new(&data.join("library.sqlite3"));
        let mut library = amp_library::demo_library();
        library.albums.truncate(2);
        library.albums[0].tracks.truncate(2);
        for (track, source) in library.albums[0].tracks.iter_mut().zip(["A", "B"]) {
            track.disc = 1;
            track.source = Some(amp_core::SourceCoordinates {
                connection: "c".into(),
                collection: "c".into(),
                item: "i".into(),
                capture: "c".into(),
                representation: source.into(),
                track: 1,
            });
        }
        let state = test_state(&services, library);
        let ui = PlayerWindow::new().unwrap();
        let adapter = Adapter::open(&data.join("collector")).unwrap();
        let _controller = Controller::new(
            &ui,
            &state,
            adapter,
            services.catalog().generation(),
            |_, done| done(Ok(())),
        )
        .unwrap();
        select(&ui, &mut state.borrow_mut(), 0);
        assert!(ui.get_show_disc_selector());
        assert_eq!(
            ui.get_album_sources().row_data(2).unwrap(),
            "Disc 1 · Version 2"
        );
        ui.invoke_album_source_selected(2);
        assert_eq!(
            displayed_tracks(&state.borrow(), 0)[0]
                .source
                .as_ref()
                .unwrap()
                .representation,
            "B"
        );
        assert_eq!(state.borrow().library.albums[0].tracks.len(), 2);
        select(&ui, &mut state.borrow_mut(), 1);
        select(&ui, &mut state.borrow_mut(), 0);
        assert_eq!(ui.get_album_source_index(), 2);
        ui.invoke_album_source_selected(2);
        assert_eq!(
            displayed_tracks(&state.borrow(), 0)[0]
                .source
                .as_ref()
                .unwrap()
                .representation,
            "B"
        );
        let target = {
            let state = state.borrow();
            let album = &state.library.albums[0];
            amp_core::browse::Target::Track {
                album: album.id.clone(),
                track: album.tracks[0].id.clone(),
            }
        };
        assert!(browse::resolve_track(&state.borrow(), &target).is_some());
        Arc::make_mut(&mut state.borrow_mut().library).albums[0]
            .tracks
            .truncate(1);
        select(&ui, &mut state.borrow_mut(), 0);
        assert!(!ui.get_show_disc_selector());
        assert_eq!(ui.get_tracks().row_count(), 1);
        drop(_controller);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn installing_refreshed_list_keeps_contents_name_and_permissions() {
        use slint::Model;
        i_slint_backend_testing::init_no_event_loop();
        let data = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let services = TestServices::new(&data.join("library.sqlite3"));
        let state = test_state(&services, Library::default());
        let ui = PlayerWindow::new().unwrap();
        let adapter = Adapter::open(&data.join("collector")).unwrap();
        let list_id = ids::CollectionListId::new();
        let id = list_id.to_string();
        let list = serde_json::from_value(serde_json::json!({
            "list": { "id": id, "collection_id": id, "revision": 1, "name": "Before", "kind": "music_tracks", "entry_count": 1, "can_edit": false },
            "entries": { "items": [{ "id": id, "position": 0, "title": "Song", "availability": { "status": "entry" } }], "next_cursor": null }
        })).unwrap();
        let mut view = View {
            connections: vec![],
            selected: None,
            snapshot: Some(
                Snapshot {
                    library: Library::default(),
                    collections: vec![],
                    lists: vec![list],
                    artwork: Default::default(),
                    album_collections: Default::default(),
                    album_revisions: Default::default(),
                    track_revisions: Default::default(),
                    artwork_descriptors: Default::default(),
                    summaries_pending: false,
                    updated: 0,
                }
                .into(),
            ),
            list: Some(list_id),
            collection: None,
            online: false,
            connection: ConnectionState::Disconnected,
            operation: None,
            list_rows: RefCell::default(),
            optimistic: None,
            recent: Vec::new(),
        };
        install(&ui, &state, &view, &[]);
        assert_eq!(ui.get_list_rows().row_count(), 1);
        let list = &mut view.snapshot.as_mut().unwrap().lists[0];
        list.list.name = "After".into();
        list.list.can_edit = true;
        list.entries.items[0].title = "Updated song".into();
        install(&ui, &state, &view, &[]);
        assert_eq!(ui.get_open_list(), 0);
        assert_eq!(ui.get_open_list_title(), "After");
        assert_eq!(ui.get_list_cards().row_data(0).unwrap().title, "After");
        assert!(ui.get_list_editable());
        assert_eq!(
            ui.get_list_rows().row_data(0).unwrap().title,
            "Updated song"
        );
        drop(adapter);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn connection_failures_and_browser_failures_show_modal_errors() {
        i_slint_backend_testing::init_no_event_loop();
        let data = std::env::temp_dir().join(format!("trash-amp-errors-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let services = TestServices::new(&data.join("library.sqlite3"));
        let state = Rc::new(RefCell::new(State {
            catalog: services.catalog(),
            folders: services.catalog().snapshot().folders,
            catalog_revision: 0,
            catalog_replacement: 0,
            library: Arc::default(),
            playback: services.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: Preferences::default(),
        }));
        let ui = PlayerWindow::new().unwrap();
        let generation = services.catalog().generation();
        let mut controller = Controller::new(
            &ui,
            &state,
            Adapter::open(&data.join("collector")).unwrap(),
            generation.clone(),
            |_, completed| completed(Err("Browser unavailable".into())),
        )
        .unwrap();
        ui.set_server_url("invalid URL".into());
        ui.invoke_connect_server();
        assert!(ui.get_error_message().contains("Invalid URL"));
        assert!(!ui.get_collector_busy());
        ui.set_error_message("".into());
        ui.set_server_url("https://example.invalid".into());
        ui.set_certificate_path("/nonexistent/trust-file".into());
        ui.invoke_connect_server();
        assert!(ui.get_error_message().contains("trust file"));
        ui.set_error_message("".into());
        let v = generation.advance();
        ui.set_collector_busy(true);
        controller
            .tx
            .send(Event::Connected(
                v,
                Err(anyhow::anyhow!("Required entitlement missing")
                    .context("Secure storage failed")),
            ))
            .unwrap();
        controller.poll();
        assert!(!ui.get_collector_busy());
        assert!(
            ui.get_error_message()
                .contains("Secure storage failed: Required entitlement missing")
        );
        assert_eq!(ui.get_connection_status(), ui.get_error_message());
        ui.set_error_message("".into());
        controller
            .tx
            .send(Event::Connected(
                v - 1,
                Err(anyhow::anyhow!("obsolete failure")),
            ))
            .unwrap();
        controller.poll();
        assert!(ui.get_error_message().is_empty());
        controller
            .tx
            .send(Event::Approval(v, "https://example.invalid/approve".into()))
            .unwrap();
        controller.poll();
        assert!(ui.get_error_message().contains("Browser unavailable"));
        assert!(
            ui.get_error_message()
                .contains("https://example.invalid/approve")
        );
        drop(controller);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn connection_checks_ignore_obsolete_sources_and_removed_credentials() {
        i_slint_backend_testing::init_no_event_loop();
        let data =
            std::env::temp_dir().join(format!("trash-amp-connection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let service = TestServices::new(&data.join("library.sqlite3"));
        let state = Rc::new(RefCell::new(State {
            catalog: service.catalog(),
            folders: service.catalog().snapshot().folders,
            catalog_revision: 0,
            catalog_replacement: 0,
            library: Arc::default(),
            playback: service.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: Preferences::default(),
        }));
        let ui = PlayerWindow::new().unwrap();
        service.catalog().register_sources(vec![], true);
        service.catalog().bootstrap_snapshot().unwrap();
        let generation = service.catalog().generation();
        generation.advance();
        generation.advance();
        let mut controller = Controller::new(
            &ui,
            &state,
            Adapter::open(&data.join("collector")).unwrap(),
            generation.clone(),
            |_, done| done(Ok(())),
        )
        .unwrap();
        let key = ConnectionKey {
            server_id: ids::ServerId::new(),
            user_id: ids::UserId::new(),
        };
        {
            let mut view = controller.view.borrow_mut();
            view.connections.push(SavedConnection {
                key,
                name: "Test".into(),
                endpoint: "https://example.invalid".into(),
                removed: false,
            });
            view.selected = Some(key);
        }
        // Keep these synthetic event checks from starting a network probe.
        controller.view.borrow_mut().operation = Some((generation.current(), u64::MAX));
        ui.set_collector_busy(true);
        controller
            .tx
            .send(Event::ConnectionChecked(1, key, ConnectionState::Lan))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_connection_kind(), ConnectionKind::Disconnected);
        controller
            .tx
            .send(Event::ConnectionChecked(2, key, ConnectionState::Lan))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_connection_kind(), ConnectionKind::Lan);
        assert!(ui.get_collector_online());
        controller
            .tx
            .send(Event::ConnectionChecked(2, key, ConnectionState::Direct))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_connection_kind(), ConnectionKind::Direct);
        assert!(ui.get_collector_online());
        ui.invoke_source_changed(0);
        controller
            .tx
            .send(Event::ConnectionChecked(2, key, ConnectionState::Public))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_connection_kind(), ConnectionKind::Disconnected);
        assert!(!ui.get_collector_online());
        {
            let mut view = controller.view.borrow_mut();
            view.selected = Some(key);
            view.connections[0].removed = true;
        }
        controller
            .tx
            .send(Event::ConnectionChecked(
                generation.current(),
                key,
                ConnectionState::Lan,
            ))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_connection_kind(), ConnectionKind::Disconnected);
        drop(controller);
        drop(state);
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn obsolete_same_target_completions_do_not_settle_reopened_album_or_list() {
        use amp_app::sources::{SourceDescriptor, SourceProvider};
        struct OfflineSource {
            id: String,
            library: Library,
        }
        impl SourceProvider for OfflineSource {
            fn descriptors(&self) -> anyhow::Result<Vec<SourceDescriptor>> {
                Ok(vec![SourceDescriptor {
                    id: self.id.clone(),
                    name: "Offline fixture".into(),
                    provider: "collector".into(),
                }])
            }
            fn load(
                &self,
                _: &str,
                _: bool,
                _: &dyn Fn() -> bool,
            ) -> anyhow::Result<amp_app::catalog::ExternalCatalog> {
                Ok(amp_app::catalog::ExternalCatalog {
                    source: self.id.clone(),
                    online: Some(false),
                    library: self.library.clone(),
                    artwork: Default::default(),
                })
            }
        }
        i_slint_backend_testing::init_no_event_loop();
        let data = std::env::temp_dir().join(format!("trash-load-fence-{}", uuid::Uuid::new_v4()));
        let services = TestServices::new(&data.join("library.sqlite3"));
        let key = ConnectionKey {
            server_id: ids::ServerId::new(),
            user_id: ids::UserId::new(),
        };
        let source = amp_collector::source_id(key);
        let mut library = amp_library::demo_library();
        library.id = source.clone();
        library.root = source.clone();
        library.albums.truncate(1);
        library.albums[0].tracks.clear();
        library.albums[0].id = amp_core::ItemId {
            library: source.clone(),
            provider: "collector".into(),
            item: ids::ItemId::new().to_string(),
        };
        let album = library.albums[0].id.clone();
        let catalog = services.catalog();
        catalog.register_sources(
            vec![Arc::new(OfflineSource {
                id: source.clone(),
                library: library.clone(),
            })],
            false,
        );
        catalog.bootstrap_snapshot().unwrap();
        catalog.select_source(source.clone());
        let started = std::time::Instant::now();
        loop {
            let snapshot = catalog.bootstrap_snapshot().unwrap();
            if snapshot.source == CatalogSource::External(source.clone()) && !snapshot.scanning {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(5));
        }
        let state = test_state(&services, library.clone());
        state.borrow_mut().selected = Some(0);
        let ui = PlayerWindow::new().unwrap();
        ui.set_home(false);
        ui.set_show_library(false);
        ui.set_show_queue(false);
        ui.set_artist_view(false);
        let mut controller = Controller::new(
            &ui,
            &state,
            Adapter::open(&data.join("collector")).unwrap(),
            catalog.generation(),
            |_, done| done(Ok(())),
        )
        .unwrap();
        controller.view.borrow_mut().selected = Some(key);
        let v = catalog.generation().current();
        // Reopening A after A/B leaves the same target but a distinct attempt.
        let obsolete_album = controller
            .album_request
            .begin((v, key, album.clone()))
            .unwrap();
        controller.album_request.cancel();
        let current_album = controller
            .album_request
            .begin((v, key, album.clone()))
            .unwrap();
        ui.set_collector_album_loading(true);
        ui.set_status("Current album load".into());
        controller
            .tx
            .send(Event::AlbumLoaded(
                v,
                key,
                album.clone(),
                obsolete_album.serial,
                Err(anyhow::anyhow!("obsolete album failure")),
            ))
            .unwrap();
        controller.poll();
        assert!(ui.get_collector_album_loading());
        assert!(!ui.get_status().contains("obsolete"));
        controller
            .tx
            .send(Event::AlbumLoaded(
                v,
                key,
                album,
                current_album.serial,
                Err(anyhow::anyhow!("current album failure")),
            ))
            .unwrap();
        controller.poll();
        assert!(!ui.get_collector_album_loading());
        assert!(ui.get_status().contains("current album failure"));
        let list_id = ids::CollectionListId::new();
        let list = api::CollectionListView {
            list: api::CollectionListSummary {
                id: list_id,
                collection_id: ids::CollectionId::new(),
                revision: ids::Revision(1),
                name: "Fixture list".into(),
                kind: ids::CollectionListKind::MusicTracks,
                entry_count: 0,
                can_edit: false,
            },
            entries: ids::Page {
                items: Vec::new(),
                next_cursor: None,
            },
        };
        let cached_id = ids::CollectionListId::new();
        let mut cached = list.clone();
        cached.list.id = cached_id;
        cached.list.name = "Cached list".into();
        cached.list.can_edit = true;
        let mut refreshed = list.clone();
        refreshed.list.name = "Refreshed earlier list".into();
        refreshed.list.revision = ids::Revision(2);
        let snapshot:Snapshot=serde_json::from_value(serde_json::json!({"library":library,"collections":[],"lists":[list,cached],"artwork":{},"album_collections":{},"updated":0})).unwrap();
        {
            let mut view = controller.view.borrow_mut();
            view.snapshot = Some(snapshot.into());
            view.list = Some(list_id);
        }
        ui.set_lists_open(true);
        ui.set_collector_list_loading(true);
        let obsolete_list = controller.list_request.begin((v, key, list_id)).unwrap();
        controller.list_request.cancel();
        let current_list = controller.list_request.begin((v, key, list_id)).unwrap();
        ui.set_status("Current list load".into());
        controller
            .tx
            .send(Event::ListLoaded(
                v,
                key,
                list_id,
                obsolete_list.serial,
                Err(anyhow::anyhow!("obsolete list failure")),
            ))
            .unwrap();
        controller.poll();
        assert!(ui.get_collector_list_loading());
        assert!(!ui.get_status().contains("obsolete"));
        controller
            .tx
            .send(Event::ListLoaded(
                v,
                key,
                list_id,
                current_list.serial,
                Err(anyhow::anyhow!("current list failure")),
            ))
            .unwrap();
        controller.poll();
        assert!(!ui.get_collector_list_loading());
        assert!(ui.get_status().contains("current list failure"));

        // A different, already cached list does not start a replacement request.
        // The earlier request can succeed even though it no longer owns the picker.
        controller.list_request.cancel();
        let pending = controller.list_request.begin((v, key, list_id)).unwrap();
        ui.set_collector_list_loading(true);
        ui.invoke_open_list_at(1);
        ui.set_status("Cached list ready".into());
        controller
            .tx
            .send(Event::ListLoaded(
                v,
                key,
                list_id,
                pending.serial,
                Ok(Box::new(refreshed)),
            ))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_open_list(), 1);
        assert_eq!(ui.get_open_list_title(), "Cached list");
        assert!(!ui.get_collector_list_loading());
        assert_eq!(ui.get_status(), "Cached list ready");
        assert!(
            pending.is_cancelled(),
            "A cached selection must cancel the previous in-flight load"
        );
        controller.view.borrow_mut().online = true;
        let (target, edit) =
            playlist_request(&controller.view.borrow(), ListAction::Delete, -1, "").unwrap();
        assert_eq!(target, key);
        let PlaylistAction::Update {
            list,
            change: PlaylistChange::Edit(api::CollectionListChange::Delete),
        } = edit
        else {
            panic!("Expected a delete for the cached selection");
        };
        assert_eq!(list.id, cached_id);
        drop(controller);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn background_collector_updates_do_not_rewrite_another_source() {
        i_slint_backend_testing::init_no_event_loop();
        let directory =
            std::env::temp_dir().join(format!("trash-source-isolation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let services = TestServices::new(&directory.join("library.sqlite3"));
        let mut library = amp_library::demo_library();
        library.albums.truncate(1);
        library.albums[0].tracks[0].id.provider = "apple-library".into();
        library.albums[0].tracks[0].availability = amp_core::Availability::Downloaded;
        let initial = services.catalog().snapshot();
        let state = Rc::new(RefCell::new(State {
            catalog: services.catalog(),
            playback: services.playback(),
            folders: initial.folders,
            catalog_revision: initial.library_revision,
            catalog_replacement: initial.replacement_revision,
            library: Arc::new(library),
            artwork: Default::default(),
            browser: Default::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: initial.preferences,
        }));
        let ui = PlayerWindow::new().unwrap();
        let mut controller = Controller::new(
            &ui,
            &state,
            Adapter::open(&directory.join("collector")).unwrap(),
            services.catalog().generation(),
            |_, done| done(Ok(())),
        )
        .unwrap();
        controller
            .tx
            .send(Event::Downloads(Ok(Vec::new())))
            .unwrap();
        let catalog = services.catalog().snapshot();
        let obsolete = ConnectionKey {
            server_id: ids::ServerId::new(),
            user_id: ids::UserId::new(),
        };
        let (tx, rx) = mpsc::channel();
        controller.source_projection = Some(rx);
        tx.send(SourceProjection {
            generation: catalog.generation,
            revision: catalog.library_revision,
            source: CatalogSource::External(amp_collector::source_id(obsolete)),
            view: Ok(SourceView {
                selected: Some(obsolete),
                connections: Vec::new(),
                catalog: None,
            }),
        })
        .unwrap();
        controller.poll();
        assert!(!ui.get_collector_active());
        assert_eq!(
            state.borrow().library.albums[0].tracks[0].availability,
            amp_core::Availability::Downloaded
        );
        drop(controller);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
