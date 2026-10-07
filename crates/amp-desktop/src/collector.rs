//! Slint presentation adapter. Every operation runs on a worker; browsing and playback have separate lifetimes.
use super::*;
use amp_collector::{Adapter, ConnectionKey, ListCommand, SavedConnection, Snapshot, api, ids};
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
}
enum Event {
    Connected(u64, anyhow::Result<ConnectionKey>),
    Refreshed(u64, ConnectionKey, anyhow::Result<Snapshot>),
    Approval(u64, String),
    Message(String),
    Mutation(u64, ConnectionKey, anyhow::Result<()>),
    Downloads,
}
pub struct Controller {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    ui: slint::Weak<PlayerWindow>,
    state: Rc<RefCell<State>>,
    adapter: Arc<Adapter>,
    view: Rc<RefCell<View>>,
    generation: Arc<AtomicU64>,
    download_busy: Arc<AtomicBool>,
}
pub fn queue_label(t: &Track) -> String {
    if t.id.provider == "collector" {
        format!("{} · {}", t.title, t.id.library)
    } else {
        t.title.clone()
    }
}
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}
fn error(ui: &PlayerWindow, result: anyhow::Result<()>) {
    if let Err(e) = result {
        ui.set_connection_status(e.to_string().into())
    }
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
                    .and_then(|path| slint::Image::load_from_path(Path::new(path)).ok())
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
    generation: Arc<AtomicU64>,
    v: u64,
    key: ConnectionKey,
) {
    std::thread::spawn(move || {
        let result = (|| {
            runtime()?.block_on(adapter.refresh(key, &|| generation.load(Ordering::Acquire) != v))
        })();
        let _ = tx.send(Event::Refreshed(v, key, result));
    });
}
impl Controller {
    pub fn new(
        ui: &PlayerWindow,
        state: &Rc<RefCell<State>>,
        data: &Path,
        generation: Arc<AtomicU64>,
    ) -> anyhow::Result<Self> {
        let adapter = Adapter::open(&data.join("collector"))?;
        state.borrow_mut().resolver = Some(adapter.clone());
        let selected = adapter.selected()?;
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
        }));
        let (tx, rx) = mpsc::channel();
        let download_busy = Arc::new(AtomicBool::new(false));
        sources(ui, &view.borrow());
        if selected.is_some() {
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
                let v = g.fetch_add(1, Ordering::AcqRel) + 1;
                let mut view = view.borrow_mut();
                view.selected = if i > 0 {
                    view.connections.get(i as usize - 1).map(|c| c.key)
                } else {
                    None
                };
                view.list = None;
                view.collection = None;
                view.online = false;
                ui.set_collection_index(0);
                ui.set_scanning(false);
                ui.set_collector_busy(false);
                error(&ui, a.select(view.selected));
                if let Some(key) = view.selected {
                    view.snapshot = a.snapshot(key).ok().flatten();
                    install(&ui, &state, &view, &a);
                    ui.set_server_url(
                        view.connections
                            .iter()
                            .find(|c| c.key == key)
                            .map(|c| c.endpoint.clone())
                            .unwrap_or_default()
                            .into(),
                    );
                    if view.connections.iter().any(|c| c.key == key && !c.removed) {
                        ui.set_collector_busy(true);
                        refresh(a.clone(), tx.clone(), g.clone(), v, key);
                    }
                } else {
                    view.snapshot = None;
                    let mut s = state.borrow_mut();
                    s.library = s.store.library().ok().flatten().unwrap_or_default();
                    s.artwork.clear();
                    s.selected = None;
                    sort_albums(&ui, &mut s);
                    albums(&ui, &s);
                    browse::rebuild(&ui, &mut s, true);
                    ui.set_collector_active(false);
                    ui.set_collector_online(false);
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
            ui.on_connect_server(move || {
                let Some(ui) = weak.upgrade() else { return };
                let url = match ui.get_server_url().parse() {
                    Ok(url) => url,
                    Err(e) => {
                        ui.set_connection_status(format!("Invalid URL: {e}").into());
                        return;
                    }
                };
                let trust = if ui.get_certificate_path().is_empty() {
                    None
                } else {
                    match read_trust(Path::new(ui.get_certificate_path().as_str())) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            ui.set_connection_status(e.to_string().into());
                            return;
                        }
                    }
                };
                ui.set_collector_busy(true);
                let v = g.fetch_add(1, Ordering::AcqRel) + 1;
                let a = a.clone();
                let tx = tx.clone();
                let g = g.clone();
                std::thread::spawn(move || {
                    let result = (|| {
                        runtime()?.block_on(a.enroll(
                            url,
                            trust,
                            &|| g.load(Ordering::Acquire) != v,
                            &mut |url| {
                                let _ = tx.send(Event::Approval(v, url));
                            },
                        ))
                    })();
                    let _ = tx.send(Event::Connected(v, result));
                });
            });
        }
        {
            let g = generation.clone();
            let weak = ui.as_weak();
            ui.on_cancel_connect(move || {
                g.fetch_add(1, Ordering::AcqRel);
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
            ui.on_refresh_server(move || {
                if let (Some(key), Some(ui)) = (view.borrow().selected, weak.upgrade()) {
                    let v = g.fetch_add(1, Ordering::AcqRel) + 1;
                    ui.set_collector_busy(true);
                    refresh(a.clone(), tx.clone(), g.clone(), v, key);
                }
            });
        }
        {
            let a = adapter.clone();
            let view = view.clone();
            let weak = ui.as_weak();
            ui.on_remove_server(move || {
                if let (Some(key), Some(ui)) = (view.borrow().selected, weak.upgrade()) {
                    error(&ui, a.remove(key));
                    if let Ok(c) = a.connections() {
                        let mut view = view.borrow_mut();
                        view.connections = c;
                        view.online = false;
                        sources(&ui, &view);
                        ui.set_collector_online(false);
                        ui.set_connection_status(
                            "Connection removed; completed downloads retained".into(),
                        );
                    }
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
                    ui.set_album_ambiguous(false)
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
                let mut s = state.borrow_mut();
                if s.player.is_none() {
                    match amp_audio::Player::with_resolver(s.resolver.clone()) {
                        Ok(p) => s.player = Some(p),
                        Err(e) => {
                            ui.set_connection_status(e.to_string().into());
                            return;
                        }
                    }
                }
                s.playlist = tracks;
                s.queued.clear();
                s.queue_start = s.playlist.len();
                s.player.as_ref().unwrap().play(s.playlist.clone(), 0);
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
            ui.on_list_command(move |verb| {
                let Some(ui) = weak.upgrade() else { return };
                if verb == 8 || verb == 9 {
                    if let Some(key) = view.borrow().selected {
                        if verb == 9 {
                            error(&ui, a.discard_commands(key));
                        } else {
                            let pending = a.pending_commands(key).unwrap_or_default();
                            let a = a.clone();
                            let tx = tx.clone();
                            let v = g.load(Ordering::Acquire);
                            std::thread::spawn(move || {
                                let result = (|| -> anyhow::Result<()> {
                                    let rt = runtime()?;
                                    for c in pending {
                                        rt.block_on(a.submit(c))?;
                                    }
                                    Ok(())
                                })();
                                let _ = tx.send(Event::Mutation(v, key, result));
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
                        let v = g.load(Ordering::Acquire);
                        std::thread::spawn(move || {
                            let result = (|| -> anyhow::Result<()> {
                                runtime()?.block_on(a.submit(command))?;
                                Ok(())
                            })();
                            let _ = tx.send(Event::Mutation(v, key, result));
                        });
                    }
                }
            });
        }
        {
            let a = adapter.clone();
            let weak = ui.as_weak();
            ui.on_save_budget(move || {
                if let Some(ui) = weak.upgrade() {
                    error(
                        &ui,
                        (|| {
                            let gib = ui.get_download_budget().parse::<f64>()?;
                            if !gib.is_finite() || gib <= 0. || gib > 1048576. {
                                anyhow::bail!("Enter a positive GiB budget")
                            };
                            a.set_budget((gib * 1024. * 1024. * 1024.) as u64)
                        })(),
                    );
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
                generation.load(Ordering::Acquire),
                key,
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
        })
    }
    pub fn poll(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        while let Ok(event) = self.rx.try_recv() {
            let current = self.generation.load(Ordering::Acquire);
            match event {
                Event::Approval(v, url) if v == current => {
                    ui.set_connection_status(
                        format!("Approve Trash Amp in your browser: {url}").into(),
                    );
                    let _ = std::process::Command::new("open").arg(url).spawn();
                }
                Event::Connected(v, result) if v == current => {
                    ui.set_collector_busy(false);
                    match result {
                        Ok(key) => {
                            let mut view = self.view.borrow_mut();
                            view.connections = self.adapter.connections().unwrap_or_default();
                            view.selected = Some(key);
                            view.snapshot = self.adapter.snapshot(key).ok().flatten();
                            view.collection = None;
                            view.list = None;
                            view.online = false;
                            error(&ui, self.adapter.select(Some(key)));
                            sources(&ui, &view);
                            install(&ui, &self.state, &view, &self.adapter);
                            ui.set_collector_busy(true);
                            refresh(
                                self.adapter.clone(),
                                self.tx.clone(),
                                self.generation.clone(),
                                v,
                                key,
                            );
                        }
                        Err(e) => ui.set_connection_status(e.to_string().into()),
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
                            install(&ui, &self.state, &view, &self.adapter);
                            render_list(&ui, &view, &self.adapter);
                        }
                        Err(e) => {
                            view.online = false;
                            ui.set_collector_online(false);
                            ui.set_connection_status(
                                format!("Cached library retained: {e}").into(),
                            );
                        }
                    }
                }
                Event::Mutation(v, key, result) if v == current => {
                    ui.set_collector_busy(false);
                    match result{Ok(())=>{ui.set_collector_busy(true);refresh(self.adapter.clone(),self.tx.clone(),self.generation.clone(),v,key)},Err(e)=>ui.set_connection_status(format!("List change unresolved: {e}. Refresh and retry the original command.").into())}
                }
                Event::Message(message) => ui.set_connection_status(message.into()),
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
