//! Slint presentation adapter. Every operation runs on a worker; browsing and playback have separate lifetimes.
use super::*;
use amp_app::catalog::{ExternalCatalog, Generation};
use amp_collector::{
    Adapter, ConnectionKey, ConnectionState, ListCommand, SavedConnection, Snapshot, api, ids,
};
use anyhow::Context;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
struct View {
    connections: Vec<SavedConnection>,
    selected: Option<ConnectionKey>,
    snapshot: Option<Snapshot>,
    list: Option<usize>,
    collection: Option<ids::CollectionId>,
    online: bool,
    connection: ConnectionState,
}
struct Connected {
    key: ConnectionKey,
    connections: Vec<SavedConnection>,
    snapshot: Option<Snapshot>,
}
fn publish_catalog(
    adapter: &Arc<Adapter>,
    catalog: &Catalog,
    version: u64,
    key: ConnectionKey,
    snapshot: Option<&Snapshot>,
) -> anyhow::Result<bool> {
    let source = format!("{}/{}", key.server_id, key.user_id);
    let data = ExternalCatalog {
        source,
        library: snapshot.map(|s| s.library.clone()).unwrap_or_default(),
        artwork: snapshot.map(|s| s.artwork.clone()).unwrap_or_default(),
    };
    catalog
        .complete_external(version, data, selection_commit(adapter.clone(), Some(key)))
        .recv()
        .context("Catalog worker stopped")?
}
fn selection_commit(
    adapter: Arc<Adapter>,
    key: Option<ConnectionKey>,
) -> amp_app::catalog::SelectionCommit {
    Arc::new(move || {
        let previous = adapter.selected()?;
        adapter.select(key)?;
        let adapter = adapter.clone();
        Ok(Box::new(move || adapter.select(previous)))
    })
}
fn open_source(
    adapter: Arc<Adapter>,
    tx: mpsc::Sender<Event>,
    generation: Generation,
    version: u64,
    key: ConnectionKey,
    catalog: Catalog,
    online: bool,
) {
    std::thread::spawn(move || {
        let result = adapter.snapshot(key).and_then(|snapshot| {
            if !publish_catalog(&adapter, &catalog, version, key, snapshot.as_ref())? {
                anyhow::bail!("Catalog operation cancelled");
            }
            Ok(snapshot)
        });
        let _ = tx.send(Event::Cached(version, key, result));
        if online {
            refresh(adapter, tx, generation, version, key, catalog);
        }
    });
}
enum Event {
    Connected(u64, anyhow::Result<Connected>),
    Cached(u64, ConnectionKey, anyhow::Result<Option<Snapshot>>),
    Refreshed(u64, ConnectionKey, anyhow::Result<Snapshot>),
    Approval(u64, String),
    OpenFailed(u64, String),
    Message(String),
    Mutation(u64, ConnectionKey, anyhow::Result<()>),
    Downloads,
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
    download_busy: Arc<AtomicBool>,
    connection_probe: Option<(u64, ConnectionKey, std::time::Instant, bool)>,
    open_url: crate::UrlOpener,
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
fn install(ui: &PlayerWindow, state: &Rc<RefCell<State>>, view: &View, adapter: &Adapter) {
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
    if let Some(snapshot) = &view.snapshot {
        s.library = snapshot.library.clone();
        if let Some(collection) = view.collection {
            s.library
                .albums
                .retain(|a| snapshot.album_collections.get(&a.id.item) == Some(&collection));
        }
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
        s.library = Library::default();
        s.artwork = BTreeMap::new();
    }
    for album in &mut s.library.albums {
        for track in &mut album.tracks {
            if adapter.downloaded(track) {
                track.availability = amp_core::Availability::Downloaded;
            }
        }
    }
    s.library.root = view
        .connections
        .iter()
        .find(|c| Some(c.key) == view.selected)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| s.library.root.clone());
    s.selected = None;
    sort_albums(ui, &mut s);
    albums(ui, &s);
    browse::rebuild(ui, &mut s, !same);
    if let Some(index) = selected.and_then(|id| s.library.albums.iter().position(|a| a.id == id)) {
        select(ui, &mut s, index);
        ui.set_show_library(showing_library);
    }
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
    ui.set_list_index(view.list.map_or(0, |i| i as i32 + 1));
    ui.set_list_entry(-1);
    ui.set_list_rows(ModelRc::default());
    ui.set_list_editable(
        view.snapshot
            .as_ref()
            .is_some_and(|s| s.collections.iter().any(|c| c.can_edit_playlists)),
    );
}
fn render_list(ui: &PlayerWindow, view: &View, adapter: &Adapter) {
    let downloads = adapter.downloads().unwrap_or_default();
    let Some(list) = view
        .snapshot
        .as_ref()
        .and_then(|s| s.lists.get(view.list.unwrap_or(usize::MAX)))
    else {
        return;
    };
    ui.set_list_name(list.list.name.clone().into());
    ui.set_list_entry(-1);
    ui.set_list_editable(list.list.can_edit);
    ui.set_list_rows(ModelRc::new(VecModel::from(
        list.entries
            .items
            .iter()
            .map(|e| BrowserRow {
                artwork: e
                    .item_id
                    .and_then(|id| view.snapshot.as_ref()?.artwork.get(&id.to_string()))
                    .and_then(|path| artwork::load(Path::new(path)))
                    .unwrap_or_default(),
                kind: 1,
                title: e.title.clone().into(),
                subtitle: format!(
                    "{} · {}",
                    e.album.as_deref().unwrap_or(""),
                    if downloads.iter().any(|d| d.status == "complete" && Some(d.remote.key) == view.selected && d.remote.collection == list.list.collection_id && matches!(&e.source, Some(ids::ListTarget::MusicTrack { capture_id, source_representation_id, track_position, .. }) if *capture_id == d.remote.capture && *source_representation_id == d.remote.source && *track_position == d.remote.position)) { "Downloaded" } else { match e.availability {
                        api::ListEntryAvailability::Available { .. } => "Streamable",
                        api::ListEntryAvailability::Preparing => "Preparing",
                        api::ListEntryAvailability::NeedsPreparation => "Needs preparation",
                        api::ListEntryAvailability::Entry => "Entry",
                        _ => "Unavailable",
                    }}
                )
                .into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
}
fn sources(ui: &PlayerWindow, view: &View) {
    ui.set_sources(ModelRc::new(VecModel::from(
        std::iter::once(SharedString::from("Local folder"))
            .chain(view.connections.iter().map(|c| SharedString::from(&c.name)))
            .collect::<Vec<_>>(),
    )));
    ui.set_source_index(
        view.selected
            .and_then(|key| view.connections.iter().position(|c| c.key == key))
            .map_or(0, |i| i as i32 + 1),
    );
}
fn refresh(
    adapter: Arc<Adapter>,
    tx: mpsc::Sender<Event>,
    generation: Generation,
    v: u64,
    key: ConnectionKey,
    catalog: Catalog,
) {
    std::thread::spawn(move || {
        let result =
            (|| runtime()?.block_on(adapter.refresh(key, &|| generation.current() != v)))();
        let result = result.and_then(|snapshot| {
            if !publish_catalog(&adapter, &catalog, v, key, Some(&snapshot))? {
                anyhow::bail!("Catalog operation cancelled");
            }
            Ok(snapshot)
        });
        let _ = tx.send(Event::Refreshed(v, key, result));
    });
}
impl Controller {
    pub fn new(
        ui: &PlayerWindow,
        state: &Rc<RefCell<State>>,
        adapter: Arc<Adapter>,
        generation: Generation,
        open_url: crate::UrlOpener,
    ) -> anyhow::Result<Self> {
        state.borrow().playback.set_resolver(adapter.clone());
        let catalog = state.borrow().catalog.clone();
        let local_adapter = adapter.clone();
        catalog.on_local_selection(selection_commit(local_adapter, None));
        let selected = if state.borrow().library.id == "demo" {
            None
        } else {
            adapter.selected()?
        };
        let connections = adapter.connections()?;
        let snapshot = selected
            .map(|key| adapter.snapshot(key))
            .transpose()?
            .flatten();
        let view = Rc::new(RefCell::new(View {
            connections,
            selected,
            snapshot,
            list: None,
            collection: None,
            online: false,
            connection: ConnectionState::Disconnected,
        }));
        let (tx, rx) = mpsc::channel();
        let download_busy = Arc::new(AtomicBool::new(false));
        sources(ui, &view.borrow());
        if let Some(key) = selected {
            let version = generation.advance();
            publish_catalog(
                &adapter,
                &catalog,
                version,
                key,
                view.borrow().snapshot.as_ref(),
            )?;
            install(ui, state, &view.borrow(), &adapter);
        }
        ui.set_download_budget(
            (adapter.budget()? as f64 / (1024. * 1024. * 1024.))
                .to_string()
                .into(),
        );
        {
            let a = adapter.clone();
            let state = state.clone();
            let view = view.clone();
            let weak = ui.as_weak();
            let g = generation.clone();
            let tx = tx.clone();
            ui.on_source_changed(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let v = g.advance();
                let mut view = view.borrow_mut();
                view.selected = if i > 0 {
                    view.connections.get(i as usize - 1).map(|c| c.key)
                } else {
                    None
                };
                view.list = None;
                view.collection = None;
                view.online = false;
                view.connection = ConnectionState::Disconnected;
                ui.set_collection_index(0);
                ui.set_scanning(false);
                ui.set_collector_busy(false);
                if let Some(key) = view.selected {
                    view.snapshot = None;
                    install(&ui, &state, &view, &a);
                    ui.set_server_url(
                        view.connections
                            .iter()
                            .find(|c| c.key == key)
                            .map(|c| c.endpoint.clone())
                            .unwrap_or_default()
                            .into(),
                    );
                    let online = view.connections.iter().any(|c| c.key == key && !c.removed);
                    ui.set_collector_busy(online);
                    open_source(
                        a.clone(),
                        tx.clone(),
                        g.clone(),
                        v,
                        key,
                        state.borrow().catalog.clone(),
                        online,
                    );
                } else {
                    view.snapshot = None;
                    let mut s = state.borrow_mut();
                    s.catalog.restore_local();
                    s.selected = None;
                    albums(&ui, &s);
                    ui.set_collector_active(false);
                    ui.set_collector_online(false);
                    ui.set_connection_kind(ConnectionKind::Disconnected);
                    ui.set_saved_lists(ModelRc::default());
                    ui.set_list_rows(ModelRc::default());
                }
            });
        }
        {
            let a = adapter.clone();
            let tx = tx.clone();
            let g = generation.clone();
            let weak = ui.as_weak();
            let catalog = catalog.clone();
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
                        let snapshot = a.snapshot(key)?;
                        if !publish_catalog(&a, &catalog, v, key, snapshot.as_ref())? {
                            anyhow::bail!("Catalog operation cancelled");
                        }
                        Ok(Connected {
                            key,
                            snapshot,
                            connections: a.connections()?,
                        })
                    });
                    let key = result.as_ref().ok().map(|ready| ready.key);
                    let _ = tx.send(Event::Connected(v, result));
                    if let Some(key) = key {
                        refresh(a, tx, g, v, key, catalog);
                    }
                });
            });
        }
        {
            let g = generation.clone();
            let weak = ui.as_weak();
            let catalog = catalog.clone();
            let view = view.clone();
            let a = adapter.clone();
            let tx = tx.clone();
            let state = state.clone();
            ui.on_cancel_connect(move || {
                let v = g.advance();
                if let Some(key) = view.borrow().selected {
                    open_source(
                        a.clone(),
                        tx.clone(),
                        g.clone(),
                        v,
                        key,
                        catalog.clone(),
                        false,
                    );
                } else if state.borrow().library.id == "demo" {
                    catalog.show_transient(state.borrow().library.clone());
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
            let a = adapter.clone();
            let tx = tx.clone();
            let g = generation.clone();
            let view = view.clone();
            let weak = ui.as_weak();
            let catalog = catalog.clone();
            ui.on_refresh_server(move || {
                if let (Some(key), Some(ui)) = (view.borrow().selected, weak.upgrade()) {
                    let v = g.advance();
                    ui.set_collector_busy(true);
                    refresh(a.clone(), tx.clone(), g.clone(), v, key, catalog.clone());
                }
            });
        }
        {
            let a = adapter.clone();
            let view = view.clone();
            let tx = tx.clone();
            let weak = ui.as_weak();
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
                    // Keychain can wait for OS authorization. Keep that wait,
                    // and its failure, off the UI thread.
                    std::thread::spawn(move || {
                        let result = a.remove(key);
                        let _ = tx.send(Event::Removed(key, result));
                    });
                }
            });
        }
        {
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            ui.on_album_source_selected(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let view = view.borrow();
                let mut s = state.borrow_mut();
                let Some(index) = s.selected else { return };
                let id = s.library.albums[index].id.clone();
                let Some(original) = view
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.library.albums.iter().find(|a| a.id == id))
                else {
                    return;
                };
                let sources = original
                    .tracks
                    .iter()
                    .filter_map(|t| t.source.as_ref().map(|r| r.representation.clone()))
                    .collect::<BTreeSet<_>>();
                s.library.albums[index] = original.clone();
                if i > 0
                    && let Some(source) = sources.iter().nth(i as usize - 1)
                {
                    s.library.albums[index].tracks.retain(|t| {
                        t.source
                            .as_ref()
                            .is_some_and(|r| &r.representation == source)
                    });
                }
                select(&ui, &mut s, index);
                ui.set_album_sources(ModelRc::new(VecModel::from(
                    std::iter::once(SharedString::from("All album discs / choose alternative"))
                        .chain(sources.iter().map(SharedString::from))
                        .collect::<Vec<_>>(),
                )));
                ui.set_album_source_index(i);
                if i > 0 {
                    ui.set_album_ambiguous(false);
                    s.playback
                        .set_default_album(Some((s.library.albums[index].tracks.clone(), false)));
                }
            });
        }
        {
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            let a = adapter.clone();
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
                install(&ui, &state, &view, &a);
            });
        }
        {
            let view = view.clone();
            let weak = ui.as_weak();
            let a = adapter.clone();
            ui.on_list_selected(move |i| {
                let Some(ui) = weak.upgrade() else { return };
                let mut view = view.borrow_mut();
                view.list = if i > 0 {
                    Some(i as usize - 1)
                } else {
                    ui.set_list_rows(ModelRc::default());
                    None
                };
                if i <= 0 {
                    return;
                }
                render_list(&ui, &view, &a);
            });
        }
        {
            let a = adapter.clone();
            let view = view.clone();
            let state = state.clone();
            let weak = ui.as_weak();
            ui.on_play_list(move |shuffle| {
                let Some(ui) = weak.upgrade() else { return };
                let view = view.borrow();
                let Some(key) = view.selected else { return };
                let Some(list) = view.list.and_then(|i| view.snapshot.as_ref()?.lists.get(i))
                else {
                    return;
                };
                let all = a.list_tracks(key, list);
                let mut tracks = all
                    .into_iter()
                    .filter(|t| a.downloaded(t) || (view.online && a.can_play(t)))
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
            ui.on_list_command(move |verb| {
                let Some(ui) = weak.upgrade() else { return };
                if verb == 8 || verb == 9 {
                    if let Some(key) = view.borrow().selected {
                        if verb == 9 {
                            let a = a.clone();
                            let tx = tx.clone();
                            catalog.persist(Box::new(move || {
                                let result = a.discard_commands(key);
                                if let Err(error) = &result {
                                    let _ = tx.send(Event::Message(error.to_string()));
                                }
                                result
                            }));
                        } else {
                            let pending = a.pending_commands(key).unwrap_or_default();
                            let a = a.clone();
                            let tx = tx.clone();
                            let v = g.current();
                            let catalog = catalog.clone();
                            let generation = g.clone();
                            std::thread::spawn(move || {
                                let result = (|| -> anyhow::Result<()> {
                                    let rt = runtime()?;
                                    for c in pending {
                                        rt.block_on(a.submit(c))?;
                                    }
                                    Ok(())
                                })();
                                let refresh_after = result.is_ok();
                                let _ = tx.send(Event::Mutation(v, key, result));
                                if refresh_after {
                                    refresh(a, tx, generation, v, key, catalog);
                                }
                            });
                        }
                    }
                    return;
                }
                let result = (|| -> anyhow::Result<_> {
                    let view = view.borrow();
                    if !view.online {
                        anyhow::bail!("List editing requires a connection")
                    };
                    let key = view.selected.context("Select server")?;
                    if !a.pending_commands(key)?.is_empty() {
                        anyhow::bail!(
                            "Retry or discard unresolved changes before making another change"
                        )
                    };
                    let snapshot = view.snapshot.as_ref().context("Refresh server")?;
                    let list = view.list.and_then(|i| snapshot.lists.get(i));
                    let collection = if verb == 0 {
                        view.collection
                            .or_else(|| {
                                snapshot
                                    .collections
                                    .iter()
                                    .find(|c| c.can_edit_playlists)
                                    .map(|c| c.id)
                            })
                            .context("Select an editable collection")?
                    } else {
                        list.context("Select list")?.list.collection_id
                    };
                    let change = match verb {
                        0 => None,
                        1 => Some(api::CollectionListChange::Rename {
                            name: ui.get_list_name().to_string(),
                        }),
                        2 => Some(api::CollectionListChange::Delete),
                        3 | 4 | 10 => {
                            let s = state.borrow();
                            let album = s
                                .selected
                                .and_then(|i| s.library.albums.get(i))
                                .context("Select album")?;
                            let track = if verb == 4 {
                                album.tracks.get(ui.get_selected_track() as usize)
                            } else {
                                album.tracks.first()
                            }
                            .context("Select track")?;
                            let amp_core::MediaAccess::Managed { item, .. } = &track.access else {
                                anyhow::bail!("Choose a server album")
                            };
                            let r: amp_collector::RemoteTrack = serde_json::from_str(item)?;
                            if r.collection != collection {
                                anyhow::bail!("List belongs to another collection")
                            };
                            Some(api::CollectionListChange::Append {
                                item_id: r.item,
                                source_representation_id: (verb != 3).then_some(r.source),
                                track_position: (verb == 4).then_some(r.position),
                            })
                        }
                        5..=7 => {
                            let list = list.context("Select list")?;
                            let i = ui.get_list_entry() as usize;
                            let entry = list.entries.items.get(i).context("Select entry")?;
                            Some(if verb == 5 {
                                api::CollectionListChange::Remove { entry_id: entry.id }
                            } else {
                                let before = if verb == 6 {
                                    list.entries
                                        .items
                                        .get(i.checked_sub(1).context("Already first")?)
                                        .map(|e| e.id)
                                } else {
                                    list.entries.items.get(i + 2).map(|e| e.id)
                                };
                                api::CollectionListChange::Move {
                                    entry_id: entry.id,
                                    before,
                                }
                            })
                        }
                        _ => anyhow::bail!("Unknown list command"),
                    };
                    Ok(ListCommand {
                        key,
                        collection,
                        list: list.map(|l| l.list.id),
                        create: (verb == 0).then(|| api::CreateCollectionList {
                            kind: None,
                            name: ui.get_list_name().to_string(),
                        }),
                        update: change.map(|change| api::UpdateCollectionList {
                            expected_revision: list.expect("existing list").list.revision,
                            change,
                        }),
                        receipt: uuid::Uuid::new_v4().to_string(),
                    })
                })();
                match result {
                    Err(e) => ui.set_connection_status(e.to_string().into()),
                    Ok(command) => {
                        ui.set_collector_busy(true);
                        let a = a.clone();
                        let tx = tx.clone();
                        let key = command.key;
                        let v = g.current();
                        let generation = g.clone();
                        let catalog = catalog.clone();
                        std::thread::spawn(move || {
                            let result = (|| -> anyhow::Result<()> {
                                runtime()?.block_on(a.submit(command))?;
                                Ok(())
                            })();
                            let refresh_after = result.is_ok();
                            let _ = tx.send(Event::Mutation(v, key, result));
                            if refresh_after {
                                refresh(a, tx, generation, v, key, catalog);
                            }
                        });
                    }
                }
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
                    Err(error) => ui.set_connection_status(error.to_string().into()),
                    Ok(bytes) => {
                        let a = a.clone();
                        let tx = tx.clone();
                        catalog.persist(Box::new(move || {
                            let result = a.set_budget(bytes);
                            if let Err(error) = &result {
                                let _ = tx.send(Event::Message(error.to_string()));
                            }
                            result
                        }));
                    }
                }
            });
        }
        {
            let a = adapter.clone();
            let state = state.clone();
            let view = view.clone();
            let weak = ui.as_weak();
            let tx = tx.clone();
            let busy = download_busy.clone();
            ui.on_download_action(move |verb| {
                let Some(ui) = weak.upgrade() else { return };
                if verb == 5 {
                    a.cancel_downloads();
                    return;
                }
                let tracks = (|| -> anyhow::Result<Vec<Track>> {
                    if verb == 4 {
                        return Ok(a
                            .downloads()?
                            .into_iter()
                            .filter(|d| d.status != "complete")
                            .map(|d| d.track)
                            .collect());
                    }
                    let view = view.borrow();
                    if verb == 2 || verb == 3 {
                        let key = view.selected.context("Select server")?;
                        let list = view
                            .list
                            .and_then(|i| view.snapshot.as_ref()?.lists.get(i))
                            .context("Select list")?;
                        Ok(a.list_tracks(key, list))
                    } else {
                        let s = state.borrow();
                        let album = s
                            .selected
                            .and_then(|i| s.library.albums.get(i))
                            .context("Select album")?;
                        if verb == 6 {
                            Ok(vec![
                                album
                                    .tracks
                                    .get(ui.get_selected_track() as usize)
                                    .context("Select track")?
                                    .clone(),
                            ])
                        } else {
                            Ok(album.tracks.clone())
                        }
                    }
                })();
                let tracks = match tracks {
                    Ok(t) => t,
                    Err(e) => {
                        ui.set_connection_status(e.to_string().into());
                        return;
                    }
                };
                if busy.swap(true, Ordering::AcqRel) {
                    ui.set_connection_status(
                        "Downloads are active; wait or cancel before changing downloads".into(),
                    );
                    return;
                }
                a.reset_download_cancel();
                let a = a.clone();
                let tx = tx.clone();
                let busy = busy.clone();
                std::thread::spawn(move || {
                    let mut failed = 0;
                    if verb != 1
                        && verb != 3
                        && let Err(e) = a.schedule(&tracks)
                    {
                        busy.store(false, Ordering::Release);
                        let _ = tx.send(Event::Message(e.to_string()));
                        return;
                    }
                    for track in tracks {
                        let result = if verb == 1 || verb == 3 {
                            a.remove_download(&track)
                        } else {
                            a.download(track)
                        };
                        if let Err(e) = result {
                            failed += 1;
                            let _ = tx.send(Event::Message(e.to_string()));
                        }
                    }
                    busy.store(false, Ordering::Release);
                    let _ = tx.send(Event::Message(format!(
                        "Downloads settled · {failed} failed"
                    )));
                    let _ = tx.send(Event::Downloads);
                });
            });
        }
        if let Some(key) = selected
            && view
                .borrow()
                .connections
                .iter()
                .any(|c| c.key == key && !c.removed)
        {
            ui.set_collector_busy(true);
            refresh(
                adapter.clone(),
                tx.clone(),
                generation.clone(),
                generation.current(),
                key,
                catalog.clone(),
            );
        }
        let pending = adapter
            .downloads()?
            .into_iter()
            .filter(|d| d.status == "pending")
            .map(|d| d.track)
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            download_busy.store(true, Ordering::Release);
            let a = adapter.clone();
            let tx = tx.clone();
            let busy = download_busy.clone();
            std::thread::spawn(move || {
                for t in pending {
                    if let Err(e) = a.download(t) {
                        let _ = tx.send(Event::Message(e.to_string()));
                    }
                }
                busy.store(false, Ordering::Release);
                let _ = tx.send(Event::Downloads);
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
            download_busy,
            connection_probe: None,
            open_url,
        })
    }
    pub fn poll(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
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
                Event::Connected(v, result) if v == current => {
                    ui.set_collector_busy(false);
                    match result {
                        Ok(ready) => {
                            let mut view = self.view.borrow_mut();
                            view.connections = ready.connections;
                            view.selected = Some(ready.key);
                            view.snapshot = ready.snapshot;
                            view.collection = None;
                            view.list = None;
                            view.online = false;
                            view.connection = ConnectionState::Disconnected;
                            sources(&ui, &view);
                            install(&ui, &self.state, &view, &self.adapter);
                            ui.set_collector_busy(true);
                        }
                        Err(e) => {
                            connection_error(&ui, format!("Could not connect to Collector: {e:#}"))
                        }
                    }
                }
                Event::Cached(v, key, result)
                    if v == current && self.view.borrow().selected == Some(key) =>
                {
                    match result {
                        Ok(snapshot) => {
                            let mut view = self.view.borrow_mut();
                            view.snapshot = snapshot;
                            install(&ui, &self.state, &view, &self.adapter);
                        }
                        Err(error) => connection_error(
                            &ui,
                            format!("Could not open the Collector library: {error:#}"),
                        ),
                    }
                }
                Event::Refreshed(v, key, result)
                    if v == current && self.view.borrow().selected == Some(key) =>
                {
                    ui.set_collector_busy(false);
                    let mut view = self.view.borrow_mut();
                    match result {
                        Ok(snapshot) => {
                            let list = view
                                .list
                                .and_then(|i| view.snapshot.as_ref()?.lists.get(i))
                                .map(|l| l.list.id);
                            view.list = list
                                .and_then(|id| snapshot.lists.iter().position(|l| l.list.id == id));
                            view.snapshot = Some(snapshot);
                            view.online = true;
                            if view.connection == ConnectionState::Disconnected {
                                view.connection = ConnectionState::Connected;
                            }
                            install(&ui, &self.state, &view, &self.adapter);
                            render_list(&ui, &view, &self.adapter);
                        }
                        Err(e) => {
                            view.online = false;
                            view.connection = ConnectionState::Disconnected;
                            ui.set_collector_online(false);
                            ui.set_connection_kind(ConnectionKind::Disconnected);
                            connection_error(
                                &ui,
                                format!(
                                    "Could not refresh the Collector library: {e:#}\n\nYour cached library is still available."
                                ),
                            );
                        }
                    }
                }
                Event::Mutation(v, _key, result) if v == current => {
                    ui.set_collector_busy(result.is_ok());
                    if let Err(error) = result {
                        ui.set_connection_status(format!("List change unresolved: {error}. Refresh and retry the original command.").into());
                    }
                }
                Event::Removed(key, result) => {
                    ui.set_removing_connection(false);
                    match result {
                        Ok(()) => {
                            let mut view = self.view.borrow_mut();
                            match self.adapter.connections() {
                                Ok(connections) => view.connections = connections,
                                Err(error) => {
                                    ui.set_connection_status(error.to_string().into());
                                    continue;
                                }
                            }
                            sources(&ui, &view);
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
                Event::Message(message) => ui.set_connection_status(message.into()),
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
                    view.online = online;
                    view.connection = status;
                    ui.set_collector_online(online);
                }
                Event::Downloads => {
                    let _ = self.download_busy.load(Ordering::Acquire);
                    let mut state = self.state.borrow_mut();
                    for album in &mut state.library.albums {
                        for track in &mut album.tracks {
                            if self.adapter.downloaded(track) {
                                track.availability = amp_core::Availability::Downloaded
                            } else if track.availability == amp_core::Availability::Downloaded {
                                track.availability = amp_core::Availability::Available
                            }
                        }
                    }
                    if let Some(index) = state.selected {
                        select(&ui, &mut state, index);
                    }
                }
                _ => {}
            }
        }
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
            library: Library::default(),
            playback: services.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
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
            library: Library::default(),
            playback: service.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
            preferences: Preferences::default(),
        }));
        let ui = PlayerWindow::new().unwrap();
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
}
