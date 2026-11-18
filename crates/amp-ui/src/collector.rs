//! Slint presentation adapter. Every operation runs on a worker; browsing and playback have separate lifetimes.
use super::*;
use amp_app::{browse::SelectionRead, catalog::Generation};
#[cfg(test)]
use amp_collector::Snapshot;
use amp_collector::{
    Adapter, AppendScope, CatalogView, ConnectionKey, ConnectionState, Download, DownloadStatus,
    PlaybackFormat, PlaylistAction, PlaylistChange, PlaylistTracks, SavedConnection, SourceView,
    api, ids,
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
}
impl View {
    fn selected_list(&self) -> Option<&api::CollectionListView> {
        let id = self.list?;
        self.snapshot
            .as_ref()?
            .lists
            .iter()
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
    Mutation(u64, ConnectionKey, bool, anyhow::Result<()>),
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
    ui.set_saved_lists(ModelRc::new(VecModel::from(
        std::iter::once(SharedString::from("Select a list"))
            .chain(
                view.snapshot
                    .as_ref()
                    .map(|s| {
                        s.lists
                            .iter()
                            .map(|l| {
                                SharedString::from(format!(
                                    "{} · {}",
                                    l.list.name,
                                    s.collections
                                        .iter()
                                        .find(|c| c.id == l.list.collection_id)
                                        .map_or("", |c| c.name.as_str())
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            )
            .collect::<Vec<_>>(),
    )));
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
    ui.set_list_index(
        view.snapshot
            .as_ref()
            .and_then(|s| s.lists.iter().position(|l| Some(l.list.id) == view.list))
            .map_or(0, |i| i as i32 + 1),
    );
    render_list(ui, view, downloads);
}
fn render_list(ui: &PlayerWindow, view: &View, downloads: &[Download]) {
    ui.set_list_entry(-1);
    let Some(list) = view.selected_list() else {
        ui.set_list_editable(
            view.snapshot
                .as_ref()
                .is_some_and(|s| s.collections.iter().any(|c| c.can_edit_playlists)),
        );
        ui.set_list_name("".into());
        ui.set_list_rows(ModelRc::default());
        return;
    };
    ui.set_list_name(list.list.name.clone().into());
    ui.set_list_editable(list.list.can_edit);
    let tracks = view
        .selected
        .map(|key| PlaylistTracks::new(key, list, downloads));
    let paths = list
        .entries
        .items
        .iter()
        .map(|e| {
            e.item_id
                .and_then(|id| view.snapshot.as_ref()?.artwork.get(&id.to_string()))
                .map(PathBuf::from)
        })
        .collect();
    ui.set_list_rows(artwork::model(
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
                BrowserRow {
                    kind: 1,
                    title: e.title.clone().into(),
                    subtitle: e.album.as_deref().unwrap_or("").into(),
                    status: status.into(),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>(),
        paths,
    ));
}
fn playlist_playable(track: &Track, online: bool) -> bool {
    track.availability == amp_core::Availability::Downloaded
        || (online && track.availability == amp_core::Availability::Available)
}
fn playlist_request(
    ui: &PlayerWindow,
    state: &State,
    view: &View,
    action: ListAction,
) -> anyhow::Result<(ConnectionKey, PlaylistAction)> {
    let key = view.selected.context("Select server")?;
    if action == ListAction::Discard {
        return Ok((key, PlaylistAction::Discard));
    }
    anyhow::ensure!(view.online, "List editing requires a connection");
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
                name: ui.get_list_name().to_string(),
            },
        ));
    }
    let list = view.selected_list().context("Select list")?;
    let change = match action {
        ListAction::Rename => PlaylistChange::Edit(api::CollectionListChange::Rename {
            name: ui.get_list_name().to_string(),
        }),
        ListAction::Delete => PlaylistChange::Edit(api::CollectionListChange::Delete),
        ListAction::AppendAlbum | ListAction::AppendDisc | ListAction::AppendTrack => {
            let album = state.selected.context("Select album")?;
            let (scope, index) = match action {
                ListAction::AppendTrack => (
                    AppendScope::Track,
                    usize::try_from(ui.get_selected_track()).context("Select track")?,
                ),
                ListAction::AppendDisc => (AppendScope::Disc, 0),
                _ => (AppendScope::Album, 0),
            };
            let track = displayed_tracks(state, album)
                .into_iter()
                .nth(index)
                .context("Select track")?;
            PlaylistChange::Append {
                track: Box::new(track),
                scope,
            }
        }
        ListAction::RemoveEntry | ListAction::MoveUp | ListAction::MoveDown => {
            let i = usize::try_from(ui.get_list_entry()).context("Select entry")?;
            let entries = &list.entries.items;
            let entry = entries.get(i).context("Select entry")?;
            PlaylistChange::Edit(if action == ListAction::RemoveEntry {
                api::CollectionListChange::Remove { entry_id: entry.id }
            } else {
                let before = if action == ListAction::MoveUp {
                    entries.get(i.checked_sub(1).context("Already first")?)
                } else {
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
        if view.borrow().selected.is_some() {
            install(ui, state, &view.borrow(), &download_records.borrow());
        }
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
            ui.on_list_selected(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let mut view = view.borrow_mut();
                let list = usize::try_from(i).ok().and_then(|index| {
                    let list = view.snapshot.as_ref()?.lists.get(index.checked_sub(1)?)?;
                    Some(list.list.id)
                });
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
        {
            let a = adapter.clone();
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            let tx = tx.clone();
            let g = generation.clone();
            let catalog = catalog.clone();
            ui.on_list_command(move |action| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.get_collector_busy() {
                    return;
                }
                let request = playlist_request(&ui, &state.borrow(), &view.borrow(), action);
                let (key, request) = match request {
                    Ok(request) => request,
                    Err(error) => {
                        ui.set_connection_status(error.to_string().into());
                        return;
                    }
                };
                let refresh = !matches!(request, PlaylistAction::Discard);
                let v = g.current();
                view.borrow_mut().operation = Some((v, catalog.snapshot().source_load_revision));
                ui.set_collector_busy(true);
                let a = a.clone();
                let tx = tx.clone();
                let catalog = catalog.clone();
                std::thread::spawn(move || {
                    let result =
                        runtime().and_then(|rt| rt.block_on(a.apply_playlist_action(key, request)));
                    let refresh_after = refresh && result.is_ok();
                    let _ = tx.send(Event::Mutation(v, key, refresh, result));
                    if refresh_after {
                        open_source(&catalog, v, key);
                    }
                });
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
                ui.set_list_index(0);
                ui.set_saved_lists(ModelRc::default());
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
        if let Some(snapshot) = &mut next
            && let Some(current) = view.selected_list()
            && let Some(incoming) = snapshot
                .lists
                .iter_mut()
                .find(|row| row.list.id == current.list.id)
            && (incoming.list.revision.0 < current.list.revision.0
                || (incoming.list.revision == current.list.revision
                    && incoming.entries.items.len() < current.entries.items.len()))
        {
            *incoming = current.clone();
        }
        view.connections = source.connections;
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
            .filter(|_| ui.get_lists_open())
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
                Event::Mutation(v, key, refresh, result)
                    if v == current && self.view.borrow().selected == Some(key) =>
                {
                    match result {
                        Err(error) => {
                            self.view.borrow_mut().operation = None;
                            ui.set_connection_status(
                                format!("Could not update playlist. {error}").into(),
                            );
                        }
                        Ok(()) if !refresh => {
                            self.view.borrow_mut().operation = None;
                            ui.set_connection_status("Unresolved list changes cleared".into());
                        }
                        Ok(()) => {}
                    }
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
        };
        install(&ui, &state, &view, &[]);
        assert_eq!(ui.get_list_rows().row_count(), 1);
        let list = &mut view.snapshot.as_mut().unwrap().lists[0];
        list.list.name = "After".into();
        list.list.can_edit = true;
        list.entries.items[0].title = "Updated song".into();
        install(&ui, &state, &view, &[]);
        assert_eq!(ui.get_list_index(), 1);
        assert_eq!(ui.get_list_name(), "After");
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
        // The earlier request must still be invalidated, including its errors.
        controller.list_request.cancel();
        let pending = controller.list_request.begin((v, key, list_id)).unwrap();
        ui.set_collector_list_loading(true);
        ui.set_list_index(2);
        ui.invoke_list_selected(2);
        ui.set_status("Cached list ready".into());
        controller
            .tx
            .send(Event::ListLoaded(
                v,
                key,
                list_id,
                pending.serial,
                Err(anyhow::anyhow!("no longer selected")),
            ))
            .unwrap();
        controller.poll();
        assert_eq!(ui.get_list_index(), 2);
        assert_eq!(ui.get_list_name(), "Cached list");
        assert!(!ui.get_collector_list_loading());
        assert_eq!(ui.get_status(), "Cached list ready");
        assert!(
            pending.is_cancelled(),
            "A cached selection must cancel the previous in-flight load"
        );
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
