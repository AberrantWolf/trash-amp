#[cfg(test)]
use amp_app::PlaybackService;
#[cfg(test)]
use amp_app::catalog::CatalogService;
use amp_app::catalog::{Catalog, Source as CatalogSource};
use amp_app::{Playback, Transport};
use amp_core::{
    AudioProcessingSettings, ColorMode, CrossfeedSettings, DumpPreference, Library, Preferences,
    Track,
};
#[cfg(test)]
use amp_library::Store;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, mpsc},
    time::Duration,
};
slint::include_modules!();

fn processing_settings(ui: &PlayerWindow) -> AudioProcessingSettings {
    AudioProcessingSettings {
        crossfeed: CrossfeedSettings {
            enabled: ui.get_crossfeed_enabled(),
            cutoff_hz: ui.get_crossfeed_cutoff().clamp(300, 2000) as u32,
            feed_db_tenths: ui.get_crossfeed_feed().clamp(10, 150) as u32,
        },
    }
}

/// A launcher supplies sandbox storage and the native capabilities it implements.
/// Absence of a picker hides folder selection without identifying an OS in the UI.
pub struct AppConfig {
    pub collector: Arc<amp_collector::Adapter>,
    pub args: Vec<String>,
    pub folder_picker: Option<FolderPicker>,
    pub file_drop: Option<FileDropInstaller>,
    pub media_controls: MediaControlsFactory,
    pub open_url: UrlOpener,
    pub music_access_settings_url: &'static str,
    pub playback: Playback,
    pub catalog: Catalog,
}

/// The host may complete selection asynchronously. Invoke the completion on the UI thread.
/// The host must retain any native access grant for as long as the library uses it.
pub type FolderPicker = Rc<dyn Fn(Box<dyn FnOnce(PathBuf)>)>;

/// The launcher owns the native implementation; the UI only retains its lifetime guard.
pub type MediaSessionGuard = Box<dyn std::any::Any>;
pub type MediaControlsFactory = fn(&slint::Window, Playback) -> Result<MediaSessionGuard, String>;
/// Hosts open external URLs and report failures without blocking the UI loop.
pub type UrlOpened = Box<dyn FnOnce(Result<(), String>) + Send>;
pub type UrlOpener = fn(&str, UrlOpened);

/// File drops are supplied by a desktop host; shared UI stays backend-independent.
pub enum FileDropEvent {
    Hover(bool),
    Files(Vec<PathBuf>),
}
/// Return whether the window is currently accepting drops, including on hover events.
pub type FileDropCallback = Rc<dyn Fn(FileDropEvent) -> bool>;
pub type FileDropInstaller = fn(&slint::Window, FileDropCallback);

mod artwork;
mod browse;
mod collector;
mod files;
struct State {
    playback: Playback,
    artwork: std::collections::BTreeMap<String, String>,
    browser: browse::Browser,
    library: Library,
    selected: Option<usize>,
    selected_sources: std::collections::BTreeMap<String, String>,
    catalog: Catalog,
    folders: Arc<[amp_library::SavedFolder]>,
    catalog_revision: u64,
    catalog_replacement: u64,
    preferences: Preferences,
}
fn time(seconds: f64) -> SharedString {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60).into()
}
fn sort_albums(ui: &PlayerWindow, state: &mut State) {
    let selected = state
        .selected
        .and_then(|i| state.library.albums.get(i))
        .map(|a| a.id.clone());
    state.library.albums.sort_by_cached_key(|a| {
        let title = a.title.to_lowercase();
        let artist = a.artist.to_lowercase();
        (title, artist, a.id.item.clone())
    });
    state.selected = selected.and_then(|id| state.library.albums.iter().position(|a| a.id == id));
    ui.set_selected_album(state.selected.map(|i| i as i32).unwrap_or(-1));
}
fn albums(ui: &PlayerWindow, state: &State) {
    state.playback.set_default_album(None);
    ui.set_selected_album(-1);
    ui.set_tracks(ModelRc::default());
    ui.set_album_title("Select an album".into());
    ui.set_album_artist(state.library.root.clone().into());
}
fn folder_choices(ui: &PlayerWindow, state: &State) {
    ui.set_folder_index(
        state
            .folders
            .iter()
            .position(|folder| folder.id == state.library.id)
            .map_or(-1, |i| i as i32),
    );
    ui.set_folder_libraries(ModelRc::new(VecModel::from(
        state
            .folders
            .iter()
            .map(|folder| SharedString::from(folder.root.as_str()))
            .collect::<Vec<_>>(),
    )));
}
fn wire_folder_selection(ui: &PlayerWindow, state: &Rc<RefCell<State>>) {
    let weak = ui.as_weak();
    let state = state.clone();
    ui.on_folder_selected(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let id = usize::try_from(index).ok().and_then(|i| {
            state
                .borrow()
                .folders
                .get(i)
                .map(|folder| folder.id.clone())
        });
        if let Some(id) = id {
            ui.set_collector_active(false);
            ui.set_device_library_active(false);
            state.borrow().catalog.open_saved_folder(id);
        } else {
            ui.set_status("Cannot reopen folder: Choose a saved folder".into());
        }
    });
}
fn wire_source_selection(ui: &PlayerWindow, state: &Rc<RefCell<State>>) {
    let weak = ui.as_weak();
    let state = state.clone();
    ui.on_source_changed(move |index| {
        let snapshot = state.borrow().catalog.snapshot();
        let Some(source) = usize::try_from(index)
            .ok()
            .and_then(|i| snapshot.sources.get(i))
        else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            ui.set_collector_active(source.provider == "collector");
            ui.set_collector_online(false);
            ui.set_connection_kind(ConnectionKind::Disconnected);
            ui.set_device_library_active(source.provider == "apple-library");
            ui.set_saved_lists(ModelRc::default());
            ui.set_list_rows(ModelRc::default());
            ui.set_collection_index(0);
        }
        state.borrow().catalog.select_source(source.id.clone());
    });
}
fn render_catalog(ui: &PlayerWindow, state: &mut State, last_notice: &mut u64) {
    let snapshot = state.catalog.snapshot();
    if snapshot.generation != state.catalog.generation().current() {
        return;
    }
    let version = snapshot.matching_version.min(i32::MAX as u64) as i32;
    if ui.get_album_matching_version() != version
        || ui.get_album_matching_busy() != snapshot.matching_busy
    {
        ui.set_album_matching_version(version);
        ui.set_album_matching_busy(snapshot.matching_busy);
        let mut rows = Vec::new();
        let mut status = if snapshot.matching_busy {
            "Reading audio and searching providers…".to_owned()
        } else {
            snapshot
                .notice
                .as_ref()
                .map(|n| n.message.clone())
                .unwrap_or_else(|| "Choose an album, then find matching releases.".to_owned())
        };
        if let Some(review) = &snapshot.matching_review {
            status = format!(
                "{}: {} possible releases{}{}",
                review.album.title,
                review.matches.len(),
                if review.truncated {
                    " · incomplete search (candidate budget reached)"
                } else {
                    ""
                },
                review
                    .artwork_error
                    .as_ref()
                    .map(|e| format!(" · artwork: {e}"))
                    .unwrap_or_default()
            );
            for candidate in &review.matches {
                let mut mappings: Vec<_> = candidate.mappings.iter().collect();
                mappings.sort_by_key(|mapping| (mapping.medium, mapping.position));
                let tracks = mappings
                    .iter()
                    .map(|m| {
                        let file = review
                            .album
                            .tracks
                            .iter()
                            .find(|t| t.id.item == m.key)
                            .map(|t| t.title.as_str())
                            .unwrap_or("File");
                        let title = candidate
                            .release
                            .media
                            .iter()
                            .find(|d| d.position == Some(m.medium))
                            .and_then(|d| d.tracks.iter().find(|t| t.position == m.position))
                            .map(|t| t.title.as_str())
                            .unwrap_or("Untitled");
                        format!("{file} → disc {}, track {}: {title}", m.medium, m.position)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let applicable = candidate.complete_tracklist
                    && !candidate.ambiguous_mapping
                    && candidate.duration_conflicts == 0
                    && candidate.mappings.len() == review.album.tracks.len()
                    && candidate.release_tracks == review.album.tracks.len();
                rows.push(AlbumMatchRow {
                    artwork: review
                        .covers
                        .get(&candidate.release.id)
                        .and_then(|p| slint::Image::load_from_path(std::path::Path::new(p)).ok())
                        .unwrap_or_default(),
                    title: candidate.release.title.clone().into(),
                    artist: candidate.release.artist.clone().into(),
                    detail: format!(
                        "{} / {} files matched · {} release tracks{}{}",
                        candidate.mappings.len(),
                        candidate.local_tracks,
                        candidate.release_tracks,
                        if candidate.ambiguous_mapping {
                            " · ambiguous assignment"
                        } else {
                            ""
                        },
                        if candidate.duration_conflicts > 0 {
                            " · duration conflicts"
                        } else {
                            ""
                        }
                    )
                    .into(),
                    tracks: tracks.into(),
                    release: candidate.release.id.clone().into(),
                    applicable,
                });
            }
        }
        if let Some(error) = &snapshot.matching_error {
            status = error.clone();
        }
        ui.set_album_matching_status(status.into());
        ui.set_album_matching_rows(ModelRc::new(VecModel::from(rows)));
    }
    if !snapshot.sources.is_empty() {
        ui.set_sources(ModelRc::new(VecModel::from(
            snapshot
                .sources
                .iter()
                .map(|s| SharedString::from(&s.name))
                .collect::<Vec<_>>(),
        )));
        let accepted = match &snapshot.source {
            CatalogSource::External(id) => id.as_str(),
            _ => "folders",
        };
        let id = snapshot.requested_source.as_deref().unwrap_or(accepted);
        ui.set_source_index(
            snapshot
                .sources
                .iter()
                .position(|s| s.id == id)
                .map_or(-1, |i| i as i32),
        );
        ui.set_device_library_active(
            snapshot
                .sources
                .iter()
                .any(|s| s.id == id && s.provider == "apple-library"),
        );
        ui.set_device_library_available(
            snapshot
                .sources
                .iter()
                .any(|s| s.provider == "apple-library"),
        );
    }
    if state.folders != snapshot.folders {
        state.folders = snapshot.folders.clone();
        folder_choices(ui, state);
    }
    ui.set_source_status(snapshot.source_message.clone().unwrap_or_default().into());
    if state.catalog_revision != snapshot.library_revision || ui.get_scan_summary().is_empty() {
        render_scan_status(
            ui,
            if snapshot.source == CatalogSource::Local {
                snapshot.library.scan.as_ref()
            } else {
                None
            },
        );
    }
    ui.set_scanning(snapshot.scanning);
    ui.set_scan_progress(
        snapshot
            .progress
            .and_then(|n| {
                n.total
                    .filter(|total| *total > 0)
                    .map(|total| n.completed as f32 / total as f32)
            })
            .unwrap_or(0.0),
    );
    let collector_catalog = matches!(&snapshot.source, CatalogSource::External(id) if snapshot.sources.iter().any(|s| &s.id == id && s.provider == "collector"));
    if !collector_catalog && state.catalog_revision != snapshot.library_revision {
        let replacement = state.catalog_replacement != snapshot.replacement_revision;
        let selected_id = state
            .selected
            .and_then(|i| state.library.albums.get(i))
            .map(|a| a.id.clone());
        let changed_selected = selected_id
            .as_ref()
            .and_then(|id| {
                state
                    .library
                    .albums
                    .iter()
                    .find(|a| &a.id == id)
                    .zip(snapshot.library.albums.iter().find(|a| &a.id == id))
            })
            .is_some_and(|(old, new)| {
                old.title != new.title
                    || old.artist != new.artist
                    || old.source_label != new.source_label
                    || old.tracks.len() != new.tracks.len()
                    || old.tracks.iter().zip(&new.tracks).any(|(a, b)| {
                        a.title != b.title
                            || a.artist != b.artist
                            || a.availability != b.availability
                            || a.blocked_reason != b.blocked_reason
                            || a.scan_status != b.scan_status
                    })
            });
        let selected_track_id = state.selected.and_then(|index| {
            usize::try_from(ui.get_selected_track())
                .ok()
                .and_then(|row| {
                    displayed_tracks(state, index)
                        .get(row)
                        .map(|track| track.id.clone())
                })
        });
        let had_selection = selected_id.is_some();
        let artwork_changed = state.artwork != *snapshot.artwork;
        state.library = (*snapshot.library).clone();
        let album_keys = state
            .library
            .albums
            .iter()
            .map(album_key)
            .collect::<std::collections::BTreeSet<_>>();
        state
            .selected_sources
            .retain(|key, _| album_keys.contains(key));
        state.artwork = (*snapshot.artwork).clone();
        state.selected = if replacement {
            None
        } else {
            selected_id.and_then(|id| state.library.albums.iter().position(|a| a.id == id))
        };
        sort_albums(ui, state);
        folder_choices(ui, state);
        if replacement || (had_selection && state.selected.is_none()) {
            albums(ui, state);
        } else if (changed_selected || artwork_changed)
            && let Some(index) = state.selected
        {
            let showing_library = ui.get_show_library();
            let showing_queue = ui.get_show_queue();
            select(ui, state, index);
            ui.set_show_library(showing_library);
            ui.set_show_queue(showing_queue);
            let track = selected_track_id
                .and_then(|id| {
                    displayed_tracks(state, index)
                        .iter()
                        .position(|t| t.id == id)
                })
                .map_or(-1, |i| i as i32);
            ui.set_selected_track(track);
        }
        browse::rebuild(ui, state, replacement);
    }
    state.catalog_revision = snapshot.library_revision;
    state.catalog_replacement = snapshot.replacement_revision;
    if let Some(notice) = snapshot.notice
        && notice.revision != *last_notice
    {
        *last_notice = notice.revision;
        ui.set_status(notice.message.into());
    }
}
fn render_scan_status(ui: &PlayerWindow, summary: Option<&amp_core::ScanSummary>) {
    let Some(summary) = summary.filter(|summary| summary.incomplete) else {
        ui.set_scan_summary(SharedString::default());
        ui.set_scan_details(SharedString::default());
        ui.set_scan_details_open(false);
        return;
    };
    ui.set_scan_summary(summary.message().into());
    let mut details = format!(
        "{} tracks added · {} tracks removed · {} tracks not verified\nYou can still try playing tracks that weren’t verified.",
        summary.added, summary.removed, summary.unverified,
    );
    for issue in summary.issues.iter().take(200) {
        details.push_str(&format!("\n\n{}\n{}", issue.path, issue.reason));
    }
    if summary.issues.len() > 200 {
        details.push_str(&format!(
            "\n\n{} more locations couldn’t be checked.",
            summary.issues.len() - 200
        ));
    }
    ui.set_scan_details(details.into());
}
fn album_key(album: &amp_core::Album) -> String {
    serde_json::to_string(&album.id).expect("album identity is serializable")
}
fn disc_sources(album: &amp_core::Album) -> Vec<(String, u32, bool)> {
    let mut sources = std::collections::BTreeMap::<String, (u32, bool)>::new();
    for track in &album.tracks {
        if let Some(source) = &track.source {
            let entry = sources
                .entry(source.representation.clone())
                .or_insert((track.disc, false));
            entry.1 |= track.playback_block().is_none();
        }
    }
    let mut sources: Vec<_> = sources
        .into_iter()
        .map(|(id, (disc, playable))| (id, disc, playable))
        .collect();
    sources.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    sources
}
fn displayed_tracks(state: &State, index: usize) -> Vec<Track> {
    let Some(album) = state.library.albums.get(index) else {
        return vec![];
    };
    let source = state.selected_sources.get(&album_key(album));
    album
        .tracks
        .iter()
        .filter(|track| {
            source.is_none_or(|id| {
                track
                    .source
                    .as_ref()
                    .is_some_and(|source| &source.representation == id)
            })
        })
        .cloned()
        .collect()
}
fn select(ui: &PlayerWindow, state: &mut State, index: usize) {
    let Some(album) = state.library.albums.get(index) else {
        return;
    };
    ui.set_album_matching_available(
        album.id.provider == "collector"
            || state.catalog.snapshot().source == CatalogSource::Local
                && album
                    .tracks
                    .iter()
                    .all(|track| matches!(track.access, amp_core::MediaAccess::File { .. })),
    );
    let sources = disc_sources(album);
    let key = album_key(album);
    if let Some(selected) = state.selected_sources.get(&key)
        && !sources.iter().any(|(id, _, _)| id == selected)
    {
        state.selected_sources.remove(&key);
    }
    // A single source disc needs no selector; alternatives stay explicit.
    let disc_count = sources
        .iter()
        .map(|(_, disc, _)| disc)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if sources.len() == 1
        && !state.selected_sources.contains_key(&key)
        && let Some((id, _, _)) = sources
            .iter()
            .find(|(_, _, playable)| *playable)
            .or(sources.first())
    {
        state.selected_sources.insert(key.clone(), id.clone());
    }
    let selected_source = state.selected_sources.get(&key);
    let ambiguous = selected_source.is_none() && disc_count < sources.len();
    let tracks = displayed_tracks(state, index);
    ui.set_album_ambiguous(ambiguous);
    state
        .playback
        .set_default_album(Some((tracks.clone(), ambiguous)));
    ui.set_album_source_index(
        selected_source
            .and_then(|id| sources.iter().position(|(source, _, _)| source == id))
            .map_or(0, |i| i as i32 + 1),
    );
    ui.set_album_sources(ModelRc::new(VecModel::from(
        std::iter::once(SharedString::from(if ambiguous {
            "Choose a disc"
        } else {
            "All discs"
        }))
        .chain(sources.iter().map(|(id, disc, _)| {
            let alternatives: Vec<_> = sources
                .iter()
                .filter(|(_, number, _)| number == disc)
                .collect();
            if alternatives.len() > 1 {
                let version = alternatives
                    .iter()
                    .position(|(source, _, _)| source == id)
                    .unwrap()
                    + 1;
                SharedString::from(format!("Disc {disc} · Version {version}"))
            } else {
                SharedString::from(format!("Disc {disc}"))
            }
        }))
        .collect::<Vec<_>>(),
    )));
    ui.set_show_disc_selector(sources.len() > 1);
    state.selected = Some(index);
    ui.set_artist_view(false);
    ui.set_show_queue(false);
    ui.set_active_track(-1);
    ui.set_selected_track(-1);
    ui.set_show_library(false);
    ui.set_selected_album(index as i32);
    ui.set_album_art(
        state
            .artwork
            .get(&album.id.item)
            .and_then(|p| artwork::load(std::path::Path::new(p)))
            .unwrap_or_default(),
    );
    ui.set_album_title(album.title.clone().into());
    ui.set_album_artist(album.artist.clone().into());
    ui.set_tracks(ModelRc::new(VecModel::from(
        tracks
            .iter()
            .map(|t| TrackRow {
                title: t.title.clone().into(),
                scan_status: t
                    .scan_status
                    .as_ref()
                    .map(|s| s.label())
                    .unwrap_or_default()
                    .into(),
                status: t
                    .blocked_reason
                    .map(|reason| reason.message())
                    .unwrap_or(match t.availability {
                        amp_core::Availability::Available | amp_core::Availability::Downloaded => {
                            ""
                        }
                        amp_core::Availability::Preparing => "Preparing",
                        amp_core::Availability::Unavailable => "Unavailable",
                        amp_core::Availability::Unsupported => "Unsupported",
                    })
                    .into(),
                duration: time(t.duration_seconds),
                number: format!("{:02}", t.number).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_status(album.source_label.clone().into());
}
fn start(ui: &PlayerWindow, state: &mut State, index: usize) {
    let Some(selected) = state.selected else {
        return;
    };
    let tracks = displayed_tracks(state, selected);
    if index >= tracks.len() {
        return;
    }
    if index == 0 && ui.get_album_ambiguous() {
        ui.set_status("Choose a source disc before playing this album".into());
        return;
    }
    if let Some(reason) = tracks[index].playback_block() {
        ui.set_status(reason.into());
        return;
    }
    state.playback.play_album(tracks, index);
    ui.set_status("Playing album".into());
}

/// Read the latest presentation snapshot. This never drives engine events,
/// transport commands, queue progression or native media metadata publication.
fn render_playback(ui: &PlayerWindow, state: &State, last_notice: &mut u64) {
    let snapshot = state.playback.snapshot();
    ui.set_adhoc_queue(
        snapshot
            .current
            .as_ref()
            .is_some_and(|t| t.id.library.starts_with("drop:")),
    );
    let upcoming = snapshot
        .up_next
        .iter()
        .filter(|track| {
            !snapshot
                .unplayable
                .iter()
                .any(|failed| failed.track.id == track.id)
        })
        .map(|track| SharedString::from(collector::queue_label(track)))
        .chain(snapshot.unplayable.iter().map(|failed| {
            SharedString::from(format!(
                "{} · Currently unplayable: {}",
                collector::queue_label(&failed.track),
                failed.reason
            ))
        }))
        .collect::<Vec<_>>();
    use slint::Model;
    if ui.get_queued().iter().collect::<Vec<_>>() != upcoming {
        ui.set_queued(ModelRc::new(VecModel::from(upcoming)));
    }
    ui.set_active_track(
        state
            .selected
            .and_then(|i| state.library.albums.get(i))
            .and_then(|_album| {
                snapshot.current.as_ref().and_then(|current| {
                    displayed_tracks(state, state.selected.unwrap())
                        .iter()
                        .position(|track| track.id == current.id)
                })
            })
            .map_or(-1, |i| i as i32),
    );
    ui.set_playing(snapshot.playing);
    ui.set_level(snapshot.level);
    if let Some(track) = &snapshot.current {
        ui.set_now_title(track.title.clone().into());
        ui.set_now_artist(track.artist.clone().into());
        ui.set_elapsed(time(snapshot.seconds));
        ui.set_duration(time(track.duration_seconds));
        if !ui.get_scrubbing() {
            ui.set_progress(if track.duration_seconds > 0.0 {
                (snapshot.seconds / track.duration_seconds).clamp(0.0, 1.0) as f32
            } else {
                0.0
            });
        }
    }
    if let Some(notice) = snapshot.notice
        && notice.revision != *last_notice
    {
        *last_notice = notice.revision;
        ui.set_status(notice.message.into());
    }
}

/// Shared Slint presentation. Platform services are supplied by the launcher.
/// The application service owns playback independently of this presentation loop.
pub fn run(config: AppConfig) -> anyhow::Result<()> {
    let AppConfig {
        args,
        collector,
        folder_picker,
        file_drop,
        media_controls,
        open_url,
        music_access_settings_url,
        playback,
        catalog,
    } = config;
    if args.iter().any(|a| a == "--help") {
        println!(
            "trash-amp [--demo] [--library PATH] [--server HTTPS_URL] [--trust PEM_OR_PAIRING_JSON]\n--demo loads a generated 2,000-album layout fixture (no audio).\nTRASH_AMP_DATA_DIR overrides the local cache directory."
        );
        return Ok(());
    }
    let initial = catalog.bootstrap_snapshot()?;
    let mut library = if args.iter().any(|a| a == "--demo") {
        amp_library::demo_library()
    } else {
        (*initial.library).clone()
    };
    if args.iter().any(|a| a == "--demo-appearances") && library.id == "demo" {
        library.albums.truncate(32);
        for album in &mut library.albums {
            album.tracks[0].artist = "The Late Hours".into();
        }
    }
    if library.id == "demo" {
        catalog.show_transient(library);
    }
    let initial = catalog.bootstrap_snapshot()?;
    let library = (*initial.library).clone();
    let preferences = initial.preferences.clone();
    let ui = PlayerWindow::new()?;
    ui.set_home(true);
    {
        let weak = ui.as_weak();
        ui.on_music_access_settings(move || {
            let weak = weak.clone();
            open_url(
                music_access_settings_url,
                Box::new(move |result| {
                    if let Err(error) = result {
                        let _ = weak
                            .upgrade_in_event_loop(move |ui| ui.set_error_message(error.into()));
                    }
                }),
            );
        });
    }

    ui.set_can_open_folder(folder_picker.is_some());
    ui.set_touch_input(cfg!(any(target_os = "ios", target_os = "android")));
    ui.global::<ButtonHints>()
        .set_hover_tooltips(!cfg!(any(target_os = "ios", target_os = "android")));
    if folder_picker.is_none() {
        ui.set_status(
            "Choose Device Music Library or connect a Collector server in Settings.".into(),
        );
    }
    ui.set_application_version(env!("CARGO_PKG_VERSION").into());
    ui.set_sort_order(i32::from(preferences.browse_artists));
    ui.set_dark_mode(preferences.color_mode == ColorMode::Dark);
    ui.set_oled_mode(preferences.oled_mode);
    ui.set_reduced_motion(preferences.reduced_motion);
    ui.set_prefer_raw(preferences.dump_preference == DumpPreference::Redumper);
    ui.set_automatic_metadata(preferences.automatic_metadata);
    ui.set_acoustid_api_key(preferences.acoustid_api_key.clone().into());
    ui.set_text_scale(preferences.text_scale);
    ui.set_audio_buffer_seconds(preferences.audio_buffer_seconds.clamp(3, 60) as i32);
    playback.set_buffer_seconds(preferences.audio_buffer_seconds);
    let processing = preferences.audio_processing.normalized();
    ui.set_crossfeed_enabled(processing.crossfeed.enabled);
    ui.set_crossfeed_cutoff(processing.crossfeed.cutoff_hz as i32);
    ui.set_crossfeed_feed(processing.crossfeed.feed_db_tenths as i32);
    playback.set_processing(processing);
    let state = Rc::new(RefCell::new(State {
        playback: playback.clone(),
        artwork: Default::default(),
        browser: browse::Browser::default(),
        library,
        selected: None,
        selected_sources: Default::default(),
        catalog: catalog.clone(),
        folders: initial.folders,
        catalog_revision: initial.library_revision,
        catalog_replacement: initial.replacement_revision,
        preferences,
    }));
    sort_albums(&ui, &mut state.borrow_mut());
    albums(&ui, &state.borrow());
    if folder_picker.is_none() && state.borrow().library.albums.is_empty() {
        ui.set_album_artist("Choose a music source on Home.".into());
    }
    browse::rebuild(&ui, &mut state.borrow_mut(), true);
    browse::wire(&ui, &state);
    let generation = catalog.generation();
    let matching_adapter = collector.clone();
    let mut collector =
        collector::Controller::new(&ui, &state, collector, generation.clone(), open_url)?;
    folder_choices(&ui, &state.borrow());
    let scan: Rc<dyn Fn(PathBuf)> = {
        let catalog = catalog.clone();
        Rc::new(move |root| catalog.scan_folder(root))
    };
    wire_folder_selection(&ui, &state);
    {
        let state = state.clone();
        ui.on_refresh_folder(move || {
            state.borrow().catalog.refresh_source();
        });
    }
    {
        let scan = scan.clone();
        let weak = ui.as_weak();
        ui.on_open_folder(move || {
            if let Some(pick) = &folder_picker {
                let weak = weak.clone();
                let scan = scan.clone();
                pick(Box::new(move |path| {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_collector_active(false);
                        ui.set_device_library_active(false);
                        ui.set_home(false);
                        scan(path);
                    }
                }));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let catalog = catalog.clone();
        ui.on_demo(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_collector_active(false);
                ui.set_device_library_active(false);
            }
            catalog.show_transient(amp_library::demo_library());
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_play_track(move |i| {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                browse::clear_track_context(&mut s);
                start(&ui, &mut s, i as usize)
            }
        });
    }
    {
        let find_state = state.clone();
        let find_ui = ui.as_weak();
        ui.on_find_album_matches(move || {
            let s = find_state.borrow();
            if let Some(album) = s.selected.and_then(|i| s.library.albums.get(i)) {
                if album.id.provider == "collector" {
                    if let Some(ui) = find_ui.upgrade() {
                        ui.set_album_matching_open(false);
                    }
                    let adapter = matching_adapter.clone();
                    let id = album.id.clone();
                    let weak = find_ui.clone();
                    std::thread::spawn(move || match adapter.album_matching_url(&id) {
                        Ok(url) => open_url(
                            url.as_str(),
                            Box::new(move |result| {
                                if let Err(error) = result {
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = weak.upgrade() {
                                            ui.set_error_message(error.into());
                                        }
                                    });
                                }
                            }),
                        ),
                        Err(error) => {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.set_error_message(error.to_string().into());
                                }
                            });
                        }
                    });
                } else {
                    s.catalog.find_album_matches(album.id.clone());
                }
            }
        });
        let catalog = catalog.clone();
        ui.on_cancel_album_matching(move || catalog.cancel_album_matching());
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_apply_album_match(move |release| {
            if let Some(ui) = weak.upgrade() {
                let s = state.borrow();
                s.catalog.apply_album_match(
                    ui.get_album_matching_version().max(0) as u64,
                    release.to_string(),
                );
            }
        });
    }
    {
        let state = state.clone();
        ui.on_play_album(move || {
            let s = state.borrow();
            if let Some(index) = s.selected {
                s.playback.play_album_start(displayed_tracks(&s, index));
            }
        });
    }
    {
        let playback = playback.clone();
        ui.on_toggle_play(move || playback.transport(Transport::Toggle));
    }
    {
        let playback = playback.clone();
        ui.on_next(move || playback.transport(Transport::Next));
    }
    {
        let playback = playback.clone();
        ui.on_previous(move || playback.transport(Transport::Previous));
    }
    {
        let playback = playback.clone();
        ui.on_seek(move |fraction| {
            playback.transport(Transport::SeekFraction(f64::from(fraction)))
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_enqueue(move |i| {
            let mut s = state.borrow_mut();
            let t = s
                .selected
                .and_then(|a| s.library.albums.get(a))
                .and_then(|_| {
                    displayed_tracks(&s, s.selected.unwrap())
                        .get(i as usize)
                        .cloned()
                });
            if let Some(t) = t
                && let Some(ui) = weak.upgrade()
            {
                browse::enqueue_track(&ui, &mut s, t);
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_clear_queue(move || {
            let s = state.borrow();
            s.playback.clear_queue();
            if let Some(ui) = weak.upgrade() {
                ui.set_queued(ModelRc::default())
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_preferences_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                s.preferences.color_mode = if ui.get_dark_mode() {
                    ColorMode::Dark
                } else {
                    ColorMode::Light
                };
                s.preferences.oled_mode = ui.get_oled_mode();
                s.preferences.text_scale = ui.get_text_scale();
                s.preferences.audio_buffer_seconds =
                    ui.get_audio_buffer_seconds().clamp(3, 60) as u32;
                s.preferences.reduced_motion = ui.get_reduced_motion();
                s.preferences.automatic_metadata = ui.get_automatic_metadata();
                s.preferences.acoustid_api_key = ui.get_acoustid_api_key().trim().to_owned();
                s.preferences.dump_preference = if ui.get_prefer_raw() {
                    DumpPreference::Redumper
                } else {
                    DumpPreference::BinCue
                };
                let processing = processing_settings(&ui);
                if s.preferences.audio_processing != processing {
                    s.preferences.audio_processing = processing;
                    s.playback.set_processing(processing);
                }
                s.catalog.save_preferences(s.preferences.clone());
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_sort_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                s.preferences.browse_artists = ui.get_sort_order() == 1;
                browse::mode_changed(&ui, &mut s);
                s.catalog.save_preferences(s.preferences.clone());
            }
        });
    }
    let file_load = files::Controller::new(&ui, state.clone(), file_drop);
    let mut last_notice = 0;
    let mut last_catalog_notice = 0;
    let timer = slint::Timer::default();
    {
        let state = state.clone();
        let weak = ui.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let size = ui.window().size().to_logical(ui.window().scale_factor());
                ui.set_wide(size.width > 720.0 * ui.get_text_scale());
                ui.set_short(size.height < 420.0 * ui.get_text_scale());
                let artwork_changed = artwork::poll();
                file_load.poll();
                collector.poll();
                let mut s = state.borrow_mut();
                browse::poll(&ui, &mut s);
                render_catalog(&ui, &mut s, &mut last_catalog_notice);
                render_playback(&ui, &s, &mut last_notice);
                if artwork_changed
                    && let Some(album) = s.selected.and_then(|index| s.library.albums.get(index))
                {
                    ui.set_album_art(
                        s.artwork
                            .get(&album.id.item)
                            .and_then(|path| artwork::load(std::path::Path::new(path)))
                            .unwrap_or_default(),
                    );
                }
            },
        );
    }
    if !args.iter().any(|a| a == "--library" || a == "--demo")
        && state
            .borrow()
            .library
            .albums
            .first()
            .is_none_or(|a| a.id.provider != "collector")
    {
        catalog.enrich_current();
    }
    if let Some(index) = args.iter().position(|a| a == "--library")
        && let Some(path) = args.get(index + 1)
    {
        ui.set_collector_active(false);
        ui.set_device_library_active(false);
        ui.set_home(false);
        scan(PathBuf::from(path))
    }
    if let Some(url) = args
        .iter()
        .position(|a| a == "--server")
        .and_then(|i| args.get(i + 1))
    {
        ui.set_server_url(url.clone().into());
        if let Some(path) = args
            .iter()
            .position(|a| a == "--trust")
            .and_then(|i| args.get(i + 1))
        {
            ui.set_certificate_path(path.clone().into());
        }
        ui.set_settings_tab(1);
        ui.set_settings(true);
        ui.invoke_connect_server();
    }
    if let Some(mode) = args
        .iter()
        .position(|a| a == "--color-mode")
        .and_then(|i| args.get(i + 1))
    {
        ui.set_dark_mode(mode != "light");
    }
    if args.iter().any(|a| a == "--oled") {
        ui.set_oled_mode(true);
    }
    if args.iter().any(|a| a == "--settings") {
        ui.set_settings(true);
    }
    if args.iter().any(|a| a == "--browse-artists") {
        state.borrow_mut().preferences.browse_artists = true;
        ui.set_sort_order(1);
        browse::mode_changed(&ui, &mut state.borrow_mut());
    }
    if let Some(query) = args
        .iter()
        .position(|a| a == "--search")
        .and_then(|i| args.get(i + 1))
    {
        ui.set_home(false);
        ui.set_search_text(query.clone().into());
        ui.invoke_search_changed();
    }
    if let Some(name) = args
        .iter()
        .position(|a| a == "--artist")
        .and_then(|i| args.get(i + 1))
    {
        ui.invoke_open_artist(name.clone().into());
    }
    // Reproducible visual QA uses the actual Slint renderer, not a separate mockup.
    if let Some(i) = args.iter().position(|a| a == "--size")
        && let Some((w, h)) = args.get(i + 1).and_then(|s| s.split_once('x'))
    {
        ui.window()
            .set_size(slint::LogicalSize::new(w.parse()?, h.parse()?));
    }
    if let Some(i) = args.iter().position(|a| a == "--text-scale")
        && let Some(value) = args.get(i + 1)
    {
        ui.set_text_scale(value.parse::<f32>()?.clamp(1.0, 2.0));
    }
    if let Some(offset) = args
        .iter()
        .position(|a| a == "--detail-scroll")
        .and_then(|i| args.get(i + 1))
    {
        ui.set_detail_y(-offset.parse::<f32>()?);
    }
    if args.iter().any(|a| a == "--select-first") {
        ui.set_home(false);
        select(&ui, &mut state.borrow_mut(), 0);
    }
    // Exercise the real local matching command in reproducible native rendering QA.
    if args.iter().any(|arg| arg == "--review-matches") {
        ui.set_album_matching_open(true);
        ui.invoke_find_album_matches();
    }
    if let Some(path) = args
        .iter()
        .position(|a| a == "--snapshot")
        .and_then(|i| args.get(i + 1))
        .cloned()
    {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_secs(1), move || {
            if let Some(ui) = weak.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(pixels) => {
                        if let Err(e) = image::save_buffer(
                            &path,
                            pixels.as_bytes(),
                            pixels.width(),
                            pixels.height(),
                            image::ColorType::Rgba8,
                        ) {
                            eprintln!("Snapshot failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("Snapshot failed: {e}"),
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    // Create the native session after the window handle exists. The session
    // calls the service directly and publishes via its host dispatcher.
    ui.show()?;
    let _media_session = match media_controls(ui.window(), playback) {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!("Media keys unavailable: {error}");
            ui.set_status(format!("Media keys unavailable: {error}").into());
            None
        }
    };
    ui.run()?;
    Ok(())
}

#[cfg(test)]
struct TestServices {
    playback: PlaybackService,
    catalog: CatalogService,
}
#[cfg(test)]
impl TestServices {
    fn new(path: &std::path::Path) -> Self {
        let playback = PlaybackService::new(Arc::new(|| Ok(()))).unwrap();
        let catalog = CatalogService::open(path, playback.playback()).unwrap();
        Self { playback, catalog }
    }
    fn playback(&self) -> Playback {
        self.playback.playback()
    }
    fn catalog(&self) -> Catalog {
        self.catalog.catalog()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_scan_banner_keeps_details_and_retry_separate_from_playback_notices() {
        use i_slint_backend_testing::ElementHandle;
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_wide(false);
        ui.set_home(true);
        ui.window().set_size(slint::LogicalSize::new(390., 740.));
        let summary = amp_core::ScanSummary {
            incomplete: true,
            source_disconnected: true,
            added: 12,
            unverified: 42,
            issues: vec![amp_core::ScanIssue {
                path: "/Volumes/Music/Album".into(),
                reason: "Connection lost".into(),
            }],
            ..Default::default()
        };
        render_scan_status(&ui, Some(&summary));
        ui.set_status("Playing album".into());
        assert!(ui.get_scan_summary().contains("no tracks removed"));
        assert!(ui.get_scan_details().contains("/Volumes/Music/Album"));
        assert!(ui.get_scan_details().contains("Connection lost"));
        assert!(!ui.get_scan_details().contains("retained"));
        let retries = Rc::new(Cell::new(0));
        let count = retries.clone();
        ui.on_refresh_folder(move || count.set(count.get() + 1));
        ui.show().unwrap();
        ElementHandle::find_by_accessible_label(&ui, "View details")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert!(ui.get_scan_details_open());
        ElementHandle::find_by_accessible_label(&ui, "Retry refresh")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(retries.get(), 1);
        assert_eq!(ui.get_error_message(), "");
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Play")
                .next()
                .unwrap()
                .accessible_enabled()
                .unwrap()
        );
        for width in [330., 390.] {
            for scale in [1., 1.6] {
                ui.window().set_size(slint::LogicalSize::new(width, 740.));
                ui.set_text_scale(scale);
                slint::platform::update_timers_and_animations();
                for label in ["Hide details", "Retry refresh", "Play"] {
                    let action = ElementHandle::find_by_accessible_label(&ui, label)
                        .next()
                        .unwrap();
                    let position = action.absolute_position();
                    let size = action.size();
                    assert!(
                        position.x >= 0. && position.x + size.width <= width,
                        "{label} fits the window width"
                    );
                    assert!(
                        position.y >= 0. && position.y + size.height <= 740.,
                        "{label} fits the window height"
                    );
                }
            }
        }
        render_scan_status(&ui, None);
        assert_eq!(ui.get_scan_summary(), "");
        assert!(!ui.get_scan_details_open());
    }
    slint::slint! {
        import { TransportSeek } from "ui/transport-seek.slint";
        export component SeekHarness inherits Window {
            in-out property <float> position <=> seek.position;
            out property <float> displayed-position: seek.displayed-position;
            out property <bool> scrubbing: seek.scrubbing;
            seek := TransportSeek { width: 100%; height: 100%; }
        }
    }
    slint::slint! {
        import { TrackItem } from "ui/track-item.slint";
        export component TrackHarness inherits Window {
            in property <bool> tap-to-play;
            callback play(); callback select(); callback enqueue();
            TrackItem { width: 100%; height: 48px; tap-to-play: root.tap-to-play; track: {title: "A track", number: "01", duration: "3:00"};
                play => { root.play(); } select => { root.select(); } enqueue => { root.enqueue(); }
            }
        }
    }
    slint::slint! {
        import { ConnectionIndicator, ConnectionKind } from "ui/connection-indicator.slint";
        export component ConnectionHarness inherits Window {
            in property <ConnectionKind> kind;
            out property <string> description: indicator.description;
            out property <bool> expanded: indicator.expanded;
            indicator := ConnectionIndicator { x: parent.width - self.width; y: parent.height - self.height; kind: root.kind; available-width: parent.width; }
        }
    }
    slint::slint! {
        import { Theme } from "ui/theme.slint";
        import { Palette } from "std-widgets.slint";
        export component AppearanceHarness inherits Window {
            in-out property <bool> dark <=> Theme.dark;
            in-out property <bool> oled <=> Theme.oled;
            out property <[color]> surfaces: [Theme.canvas, Theme.surface, Theme.raised, Theme.transport];
            out property <color> text: Theme.text;
            out property <color> selection: Theme.selection;
        }
    }
    #[test]
    fn local_album_review_controls_reopen_cancel_and_accept_only_applicable_rows() {
        use i_slint_backend_testing::ElementHandle;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(390., 844.));
        ui.set_selected_album(0);
        ui.set_album_matching_available(true);
        ui.set_album_title("Untagged folder".into());
        ui.set_album_matching_busy(true);
        ui.show().unwrap();
        let requested = Rc::new(RefCell::new(0));
        let count = requested.clone();
        ui.on_find_album_matches(move || *count.borrow_mut() += 1);
        let activate = |label| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap()
                .invoke_accessible_default_action();
            slint::platform::update_timers_and_animations();
        };
        activate("Find matching albums");
        assert!(ui.get_album_matching_open());
        activate("Close");
        assert!(!ui.get_album_matching_open());
        activate("Find matching albums");
        assert_eq!(*requested.borrow(), 2, "pending work remains reopenable");
        let cancelled = Rc::new(RefCell::new(false));
        let value = cancelled.clone();
        ui.on_cancel_album_matching(move || *value.borrow_mut() = true);
        activate("Cancel matching");
        assert!(*cancelled.borrow());
        ui.set_album_matching_busy(false);
        ui.set_album_matching_rows(ModelRc::new(VecModel::from(vec![
            AlbumMatchRow {
                title: "Complete release".into(),
                release: "chosen".into(),
                tracks: "01.flac → disc 1, track 1".into(),
                applicable: true,
                ..Default::default()
            },
            AlbumMatchRow {
                title: "Partial release".into(),
                applicable: false,
                ..Default::default()
            },
        ])));
        slint::platform::update_timers_and_animations();
        let partial = ElementHandle::find_by_accessible_label(&ui, "Needs manual mapping")
            .next()
            .unwrap();
        assert_eq!(partial.accessible_enabled(), Some(false));
        let chosen = Rc::new(RefCell::new(String::new()));
        let value = chosen.clone();
        ui.on_apply_album_match(move |release| *value.borrow_mut() = release.to_string());
        activate("Use this album");
        assert_eq!(*chosen.borrow(), "chosen");
        ui.set_album_matching_busy(true);
        slint::platform::update_timers_and_animations();
        assert_eq!(
            ElementHandle::find_by_accessible_label(&ui, "Use this album")
                .next()
                .unwrap()
                .accessible_enabled(),
            Some(false)
        );
        for (width, scale) in [(390., 1.), (390., 1.5), (980., 1.)] {
            ui.window().set_size(slint::LogicalSize::new(width, 844.));
            ui.set_text_scale(scale);
            slint::platform::update_timers_and_animations();
            let button = ElementHandle::find_by_accessible_label(&ui, "Use this album")
                .next()
                .unwrap();
            assert!(
                button.absolute_position().x >= 0.
                    && button.absolute_position().x + button.size().width
                        <= ui.window().size().width as f32,
                "matching action overflows the window"
            );
        }
    }
    #[test]
    fn catalog_metadata_refresh_keeps_up_next_open() {
        struct MetadataBackend;
        impl amp_app::catalog::Backend for MetadataBackend {
            fn scan(
                &self,
                _: PathBuf,
                _: Preferences,
                _: &dyn Fn() -> bool,
                _: &mut dyn FnMut(amp_library::ScanProgress),
            ) -> anyhow::Result<Library> {
                unreachable!("This test only requests metadata enrichment")
            }
            fn enrich(
                &self,
                library: &mut Library,
                _: &Store,
                _: &dyn Fn() -> bool,
                _: &dyn Fn() -> bool,
                update: &mut dyn FnMut(&amp_core::Album),
            ) -> anyhow::Result<()> {
                library.albums[0].title = "Enriched title".into();
                library.albums[0].tracks[0].title = "Enriched track".into();
                update(&library.albums[0]);
                Ok(())
            }
        }
        i_slint_backend_testing::init_no_event_loop();
        let data =
            std::env::temp_dir().join(format!("trash-queue-metadata-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let cache = data.join("library.sqlite3");
        let mut library = amp_library::demo_library();
        library.albums.truncate(1);
        library.id = "review-queue".into();
        library.root = "/offline/review-queue".into();
        Store::open(&cache).unwrap().save_library(&library).unwrap();
        let playback = PlaybackService::new(Arc::new(|| Ok(()))).unwrap();
        let service =
            CatalogService::with_backend(&cache, playback.playback(), Arc::new(MetadataBackend))
                .unwrap();
        let catalog = service.catalog();
        let snapshot = catalog.bootstrap_snapshot().unwrap();
        let mut state = State {
            playback: playback.playback(),
            catalog: catalog.clone(),
            library: (*snapshot.library).clone(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
            selected_sources: Default::default(),
            folders: snapshot.folders,
            catalog_revision: snapshot.library_revision,
            catalog_replacement: snapshot.replacement_revision,
            preferences: snapshot.preferences,
        };
        let ui = PlayerWindow::new().unwrap();
        browse::rebuild(&ui, &mut state, true);
        select(&ui, &mut state, 0);
        browse::show_playback_queue(&ui, &mut state);
        assert!(ui.get_show_queue());
        catalog.enrich_current();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut notice = 0;
        while state.library.albums[0].title != "Enriched title" {
            render_catalog(&ui, &mut state, &mut notice);
            assert!(std::time::Instant::now() < deadline, "Metadata completes");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ui.get_show_queue(),
            "Updating the selected album must not leave Up next"
        );
        assert!(!ui.get_show_library());
        assert_eq!(ui.get_album_title(), "Enriched title");
        drop(state);
        drop(service);
        drop(playback);
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn icon_button_preserves_input_accessibility_and_suppresses_touch_hints() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_wide(false);
        ui.set_home(true);

        ui.set_collector_active(true);
        ui.window().set_size(slint::LogicalSize::new(390., 740.));
        let clicks = Rc::new(Cell::new(0));
        let count = clicks.clone();
        ui.on_refresh_server(move || count.set(count.get() + 1));
        ui.show().unwrap();
        let button = ElementHandle::find_by_accessible_label(&ui, "Refresh")
            .next()
            .unwrap();
        assert_eq!(button.accessible_enabled(), Some(true));
        let has_hint = || {
            ElementHandle::find_by_element_id(&ui, "IconButton::hint")
                .next()
                .is_some()
        };
        let position = slint::LogicalPosition::new(
            button.absolute_position().x + 22.,
            button.absolute_position().y + 22.,
        );
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        assert!(
            !has_hint(),
            "Hints wait for hover rather than flashing immediately"
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
        assert!(has_hint());
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(300., 140.),
        });
        assert!(!has_hint(), "Leaving the button dismisses its hint");
        ui.global::<ButtonHints>().set_hover_tooltips(false);
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
        assert!(!has_hint(), "Touch hosts must never create hover popups");
        assert_eq!(clicks.get(), 1);
        for text in [slint::SharedString::from(Key::Return), " ".into()] {
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text });
        }
        button.invoke_accessible_default_action();
        assert_eq!(clicks.get(), 4);
        ui.set_collector_busy(true);
        assert_eq!(button.accessible_enabled(), Some(false));
        button.invoke_accessible_default_action();
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert_eq!(clicks.get(), 4, "Disabled actions ignore activation");
    }

    #[test]
    fn phone_actions_fit_and_leave_space_for_tracks_at_large_text_sizes() {
        use i_slint_backend_testing::ElementHandle;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_wide(false);
        ui.set_can_open_folder(false);
        ui.set_collector_active(true);
        ui.set_collector_online(true);
        ui.set_show_library(false);
        ui.set_sources(ModelRc::new(VecModel::from(vec![
            "A long Collector server name".into(),
        ])));
        ui.set_collections(ModelRc::new(VecModel::from(vec![
            "Music collection".into(),
        ])));
        ui.set_album_title("Evening Sessions".into());
        ui.set_album_artist("The Late Hours".into());
        ui.set_tracks(ModelRc::new(VecModel::from(vec![super::TrackRow {
            title: "First track".into(),
            number: "01".into(),
            duration: "3:00".into(),
            ..Default::default()
        }])));
        ui.show().unwrap();
        for width in [330., 390.] {
            for scale in [1., 1.6] {
                ui.window().set_size(slint::LogicalSize::new(width, 740.));
                ui.set_text_scale(scale);
                slint::platform::update_timers_and_animations();
                for label in [
                    "Home",
                    "Back",
                    "Play album",
                    "Download album",
                    "Download track",
                    "Remove download",
                ] {
                    let button = ElementHandle::find_by_accessible_label(&ui, label)
                        .next()
                        .unwrap();
                    let position = button.absolute_position();
                    let size = button.size();
                    assert!(
                        size.width >= 44. && size.height >= 44.,
                        "{label} keeps a touch target"
                    );
                    assert!(
                        position.x >= 0. && position.x + size.width <= width,
                        "{label} fits {width}px at {scale}x text"
                    );
                }
                for label in ["Music source", "Music collection", "Settings"] {
                    assert!(
                        ElementHandle::find_by_accessible_label(&ui, label)
                            .next()
                            .is_none(),
                        "{label} belongs on Home"
                    );
                }
                let tracks = ElementHandle::find_by_accessible_label(&ui, "Album tracks")
                    .next()
                    .unwrap();
                assert!(
                    tracks.size().height >= 220.,
                    "Track space at {width}px/{scale}x: {}",
                    tracks.size().height
                );
            }
        }
    }

    #[test]
    fn oled_only_blacks_dark_surfaces_and_preserves_content_states() {
        use slint::Model;
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppearanceHarness::new().unwrap();
        let black = slint::Color::from_rgb_u8(0, 0, 0);
        let dark_text = ui.get_text();
        let dark_selection = ui.get_selection();
        assert!(ui.get_surfaces().iter().all(|c| c != black));
        ui.set_oled(true);
        assert!(ui.get_surfaces().iter().all(|c| c == black));
        assert_eq!(ui.get_text(), dark_text);
        assert_eq!(ui.get_selection(), dark_selection);
        ui.set_dark(false);
        let light = ui.get_surfaces().iter().collect::<Vec<_>>();
        assert!(light.iter().all(|c| *c != black));
        ui.set_oled(false);
        assert_eq!(light, ui.get_surfaces().iter().collect::<Vec<_>>());
        ui.set_dark(true);
        assert!(ui.get_surfaces().iter().all(|c| c != black));
    }
    #[test]
    fn appearance_selection_saves_light_and_dark_in_both_directions() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(390., 844.));
        ui.set_settings(true);
        let saved = Rc::new(RefCell::new(Vec::new()));
        let values = saved.clone();
        let weak = ui.as_weak();
        ui.on_preferences_changed(move || {
            values
                .borrow_mut()
                .push(weak.upgrade().unwrap().get_dark_mode());
        });
        ui.show().unwrap();
        let combo = ElementHandle::find_by_accessible_label(&ui, "Color mode")
            .next()
            .unwrap();
        for (key, expected) in [
            (Key::UpArrow, false),
            (Key::DownArrow, true),
            (Key::UpArrow, false),
            (Key::DownArrow, true),
        ] {
            let position = slint::LogicalPosition::new(
                combo.absolute_position().x + combo.size().width / 2.,
                combo.absolute_position().y + combo.size().height / 2.,
            );
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            for text in [slint::SharedString::from(key), Key::Return.into()] {
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text });
            }
            assert_eq!(ui.get_dark_mode(), expected);
            assert_eq!(saved.borrow().last(), Some(&expected));
        }
        assert_eq!(*saved.borrow(), vec![false, true, false, true]);
    }
    #[test]
    fn crossfeed_real_preset_and_slider_inputs_update_settings_and_preset_label() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(980., 1000.));
        ui.set_settings(true);
        ui.set_crossfeed_enabled(true);
        ui.show().unwrap();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let values = saved.clone();
        let weak = ui.as_weak();
        ui.on_preferences_changed(move || {
            values
                .borrow_mut()
                .push(processing_settings(&weak.unwrap()));
        });
        let combo = ElementHandle::find_by_accessible_label(&ui, "Crossfeed preset")
            .next()
            .unwrap();
        for (key, expected) in [
            (Key::DownArrow, 60),
            (Key::DownArrow, 95),
            (Key::UpArrow, 60),
        ] {
            let position = slint::LogicalPosition::new(
                combo.absolute_position().x + combo.size().width / 2.,
                combo.absolute_position().y + combo.size().height / 2.,
            );
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            for text in [slint::SharedString::from(key), Key::Return.into()] {
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text });
            }
            assert_eq!(ui.get_crossfeed_feed(), expected);
            assert_eq!(
                saved.borrow().last().unwrap().crossfeed.feed_db_tenths,
                expected as u32
            );
        }
        assert_eq!(combo.accessible_value().as_deref(), Some("Chu Moy"));
        let cutoff = ElementHandle::find_by_accessible_label(&ui, "Crossfeed cutoff in Hz")
            .next()
            .unwrap();
        cutoff.invoke_accessible_increment_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(ui.get_crossfeed_cutoff(), 701);
        assert_eq!(saved.borrow().last().unwrap().crossfeed.cutoff_hz, 701);
        assert_eq!(combo.accessible_value().as_deref(), Some("Custom"));
        cutoff.invoke_accessible_decrement_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(ui.get_crossfeed_cutoff(), 700);
        assert_eq!(combo.accessible_value().as_deref(), Some("Chu Moy"));
        let feed =
            ElementHandle::find_by_accessible_label(&ui, "Crossfeed attenuation in tenths of a dB")
                .next()
                .unwrap();
        feed.invoke_accessible_increment_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(ui.get_crossfeed_feed(), 61);
        assert_eq!(saved.borrow().last().unwrap().crossfeed.feed_db_tenths, 61);
        assert_eq!(combo.accessible_value().as_deref(), Some("Custom"));
        ui.invoke_select_crossfeed_preset(2);
        slint::platform::update_timers_and_animations();
        assert_eq!(ui.get_crossfeed_cutoff(), 650);
        assert_eq!(ui.get_crossfeed_feed(), 95);
        assert_eq!(cutoff.accessible_value().as_deref(), Some("650"));
        assert_eq!(feed.accessible_value().as_deref(), Some("95"));
        assert_eq!(combo.accessible_value().as_deref(), Some("Jan Meier"));
        feed.invoke_accessible_increment_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(ui.get_crossfeed_feed(), 96);
        assert_eq!(combo.accessible_value().as_deref(), Some("Custom"));
        ui.set_crossfeed_enabled(false);
        assert_eq!(cutoff.accessible_enabled(), Some(false));
        assert_eq!(feed.accessible_enabled(), Some(false));
    }

    #[test]
    fn crossfeed_presets_tuning_and_toggle_preserve_saved_values() {
        use i_slint_backend_testing::ElementHandle;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(390., 844.));
        ui.set_settings(true);
        ui.show().unwrap();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let captured = saved.clone();
        let weak = ui.as_weak();
        ui.on_preferences_changed(move || {
            captured
                .borrow_mut()
                .push(processing_settings(&weak.unwrap()));
        });
        assert_eq!(processing_settings(&ui), AudioProcessingSettings::default());
        assert_eq!(ui.get_crossfeed_preset(), 0);
        let toggle = ElementHandle::find_by_accessible_label(&ui, "Binaural crossfeed")
            .next()
            .unwrap();
        toggle.invoke_accessible_default_action();
        assert!(saved.borrow().last().unwrap().crossfeed.enabled);
        for (preset, cutoff, feed) in [(0, 700, 45), (1, 700, 60), (2, 650, 95)] {
            ui.invoke_select_crossfeed_preset(preset);
            assert_eq!(ui.get_crossfeed_preset(), preset);
            assert_eq!(ui.get_crossfeed_cutoff(), cutoff);
            assert_eq!(ui.get_crossfeed_feed(), feed);
            assert_eq!(
                saved.borrow().last().unwrap().crossfeed.feed_db_tenths,
                feed as u32
            );
        }
        ui.set_crossfeed_cutoff(1234);
        ui.set_crossfeed_feed(78);
        ui.invoke_preferences_changed();
        assert_eq!(ui.get_crossfeed_preset(), 3);
        ui.invoke_select_crossfeed_preset(3);
        toggle.invoke_accessible_default_action();
        assert!(!ui.get_crossfeed_enabled());
        assert_eq!(ui.get_crossfeed_cutoff(), 1234);
        assert_eq!(ui.get_crossfeed_feed(), 78);
        let combo = ElementHandle::find_by_accessible_label(&ui, "Crossfeed preset")
            .next()
            .unwrap();
        assert_eq!(combo.accessible_enabled(), Some(false));
        toggle.invoke_accessible_default_action();
        assert_eq!(combo.accessible_enabled(), Some(true));
        assert_eq!(ui.get_crossfeed_preset(), 3);
        assert_eq!(
            saved.borrow().last().unwrap().crossfeed,
            CrossfeedSettings {
                enabled: true,
                cutoff_hz: 1234,
                feed_db_tenths: 78,
            }
        );
    }

    #[test]
    fn appearance_checkbox_keeps_pointer_and_accessibility_behavior() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{PointerEventButton, WindowEvent};
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(390., 844.));
        ui.set_settings(true);
        ui.show().unwrap();
        let checkbox = ElementHandle::find_by_accessible_label(&ui, "OLED black backgrounds")
            .next()
            .unwrap();
        let position = slint::LogicalPosition::new(
            checkbox.absolute_position().x + 9.,
            checkbox.absolute_position().y + checkbox.size().height / 2.,
        );
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert!(ui.get_oled_mode());
        checkbox.invoke_accessible_default_action();
        assert!(!ui.get_oled_mode());
    }
    #[test]
    fn error_modal_blocks_transport_and_dismisses_with_keyboard() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(390.0, 844.0));
        let toggled = Rc::new(Cell::new(0));
        let count = toggled.clone();
        ui.on_toggle_play(move || count.set(count.get() + 1));
        ui.show().unwrap();
        let play = ElementHandle::find_by_accessible_label(&ui, "Play")
            .next()
            .unwrap();
        let position = slint::LogicalPosition::new(
            play.absolute_position().x + play.size().width / 2.0,
            play.absolute_position().y + play.size().height / 2.0,
        );
        ui.set_error_message("The server could not be reached. Retry your connection.".into());
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Dismiss error")
                .next()
                .is_some()
        );
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert_eq!(toggled.get(), 0);
        assert!(
            !ui.get_error_message().is_empty(),
            "outside clicks do not dismiss the error"
        );
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Escape.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Escape.into(),
        });
        assert!(ui.get_error_message().is_empty());
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
        assert_eq!(toggled.get(), 1);
    }

    #[test]
    fn connection_indicator_hover_click_keyboard_and_states() {
        use slint::{
            LogicalPosition, LogicalSize,
            platform::{Key, PointerEventButton, WindowEvent},
        };
        i_slint_backend_testing::init_no_event_loop();
        let ui = ConnectionHarness::new().unwrap();
        ui.window().set_size(LogicalSize::new(300., 100.));
        ui.show().unwrap();
        assert!(ui.get_description().contains("Not connected"));
        for (kind, label) in [
            (ConnectionKind::Public, "Public streaming"),
            (ConnectionKind::Lan, "LAN streaming"),
            (ConnectionKind::Direct, "Direct internet streaming"),
            (ConnectionKind::Unavailable, "Streaming unavailable"),
            (ConnectionKind::Checking, "Streaming route unconfirmed"),
        ] {
            ui.set_kind(kind);
            assert!(ui.get_description().contains(label));
        }
        let position = LogicalPosition::new(288., 91.);
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        assert!(ui.get_expanded());
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(5., 5.),
        });
        assert!(!ui.get_expanded());
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(5., 5.),
        });
        assert!(
            ui.get_expanded(),
            "Click keeps the explanation open after the pointer leaves"
        );
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Escape.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Escape.into(),
        });
        assert!(!ui.get_expanded());
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Return.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Return.into(),
        });
        assert!(ui.get_expanded());
    }
    #[test]
    fn saved_folder_selector_reopens_offline_and_cancels_pending_scans() {
        use i_slint_backend_testing::ElementHandle;
        i_slint_backend_testing::init_no_event_loop();
        let data = std::env::temp_dir().join(format!("trash-amp-folders-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let store = Store::open(&data.join("library.sqlite3")).unwrap();
        for id in ["a", "b"] {
            store
                .save_library(&Library {
                    id: id.into(),
                    root: format!("/offline/{id}"),
                    ..Library::default()
                })
                .unwrap();
        }
        let service = TestServices::new(&data.join("library.sqlite3"));
        let state = Rc::new(RefCell::new(State {
            library: store.library().unwrap().unwrap(),
            catalog: service.catalog(),
            folders: service.catalog().snapshot().folders,
            catalog_revision: 0,
            catalog_replacement: 0,
            playback: service.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: Preferences::default(),
        }));
        let ui = PlayerWindow::new().unwrap();
        let generation = service.catalog().generation();
        let controller = collector::Controller::new(
            &ui,
            &state,
            amp_collector::Adapter::open(&data.join("collector")).unwrap(),
            generation.clone(),
            |_, done| done(Ok(())),
        )
        .unwrap();
        folder_choices(&ui, &state.borrow());
        wire_folder_selection(&ui, &state);
        ui.set_home(true);

        ui.show().unwrap();
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Saved folder library")
                .next()
                .is_some()
        );
        assert_eq!(ui.get_folder_index(), 1);
        ui.set_scanning(true);
        ui.invoke_folder_selected(0);
        let wait_catalog = |id: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut notice = 0;
            while state.borrow().library.id != id {
                render_catalog(&ui, &mut state.borrow_mut(), &mut notice);
                assert!(
                    std::time::Instant::now() < deadline,
                    "catalog selection completes"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_catalog("a");
        assert!(!ui.get_scanning());
        assert!(generation.current() > 0);
        assert_eq!(state.borrow().library.root, "/offline/a");
        assert_eq!(store.library().unwrap().unwrap().id, "a");
        assert_eq!(ui.get_folder_index(), 0);
        ui.invoke_folder_selected(1);
        wait_catalog("b");
        assert_eq!(state.borrow().library.root, "/offline/b");
        assert_eq!(store.saved_folders().unwrap().len(), 2);
        ui.invoke_folder_selected(-1);
        assert_eq!(state.borrow().library.id, "b");
        assert!(ui.get_status().starts_with("Cannot reopen folder"));
        drop(controller);
        drop(ui);
        drop(state);
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn settings_and_lists_replace_library_and_preserve_selection() {
        use i_slint_backend_testing::ElementHandle;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
        ui.set_selected_album(7);
        ui.set_selected_track(3);
        ui.set_search_text("Miles".into());
        ui.show().unwrap();
        let has_browser = || {
            ElementHandle::find_by_accessible_label(&ui, "Album browser")
                .next()
                .is_some()
        };
        assert!(has_browser());
        ui.set_lists_open(true);
        slint::platform::update_timers_and_animations();
        assert!(!has_browser());
        ui.set_settings(true);
        slint::platform::update_timers_and_animations();
        assert!(!ui.get_lists_open());
        assert!(!has_browser());
        ui.set_settings_tab(1);
        slint::platform::update_timers_and_animations();
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Collector server URL")
                .next()
                .is_some()
        );
        ui.set_settings_tab(2);
        slint::platform::update_timers_and_animations();
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Download budget in GiB")
                .next()
                .is_some()
        );
        ui.set_lists_open(true);
        slint::platform::update_timers_and_animations();
        assert!(!ui.get_settings());
        ui.set_lists_open(false);
        slint::platform::update_timers_and_animations();
        assert!(has_browser());
        assert_eq!(ui.get_selected_album(), 7);
        assert_eq!(ui.get_selected_track(), 3);
        assert_eq!(ui.get_search_text(), "Miles");
    }
    #[test]
    fn home_owns_source_controls_and_restores_the_current_library() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{Key, WindowEvent};
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_wide(false);
        ui.set_can_open_folder(false);
        ui.set_collector_active(true);
        ui.set_sources(ModelRc::new(VecModel::from(vec![
            "Local folder".into(),
            "Collector".into(),
        ])));
        ui.set_collections(ModelRc::new(VecModel::from(vec![
            "All collections".into(),
            "Music".into(),
        ])));
        ui.set_tracks(ModelRc::new(VecModel::from(
            (0..32)
                .map(|i| super::TrackRow {
                    title: format!("Track {i}").into(),
                    number: format!("{i:02}").into(),
                    duration: "3:00".into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_track_y(-240.);
        ui.set_selected_album(7);
        ui.set_selected_track(3);
        ui.set_search_text("Miles".into());
        ui.set_show_library(false);
        ui.set_home(true);

        ui.window().set_size(slint::LogicalSize::new(390., 844.));
        ui.show().unwrap();
        let activate = |label| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap()
                .invoke_accessible_default_action();
            slint::platform::update_timers_and_animations();
        };
        let has = |label| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        assert!(has("Home page"));
        assert!(has("Music source") && has("Music collection"));
        assert!(!has("Album tracks") && !has("Album browser"));
        activate("Settings");
        assert!(ui.get_settings() && !ui.get_home());
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Escape.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Escape.into(),
        });
        assert!(ui.get_home() && !ui.get_settings());
        activate("Manage servers");
        assert_eq!(ui.get_settings_tab(), 1);
        assert!(has("Collector server URL"));
        activate("Home");
        activate("Lists");
        assert!(ui.get_lists_open() && !ui.get_home());
        activate("Home");
        activate("Browse library");
        assert!(!ui.get_home());
        assert!(has("Album tracks"));
        assert!(!has("Music source") && !has("Music collection"));
        assert_eq!(ui.get_selected_album(), 7);
        assert_eq!(ui.get_selected_track(), 3);
        assert_eq!(ui.get_search_text(), "Miles");
        assert!(!ui.get_show_library(), "Home returns to the existing album");
        assert!(
            (ui.get_track_y() + 240.).abs() < 1.,
            "Home preserves the track scroll position"
        );
        activate("Home");
        activate("Browse library");
        assert!(has("Album tracks"));
        assert_eq!(ui.get_selected_track(), 3);
        assert!((ui.get_track_y() + 240.).abs() < 1.);
        ui.set_show_library(true);
        let browser = ElementHandle::find_by_accessible_label(&ui, "Album browser")
            .next()
            .unwrap();
        assert!(
            (browser.absolute_position().y - 16.).abs() < 1.,
            "No toolbar above browsing"
        );
        assert!(
            browser.size().height > 650.,
            "Browsing uses the former source-picker space"
        );
    }

    #[test]
    fn album_pane_restores_desktop_width_and_remembers_drag_across_phone_mode() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::PointerEventButton;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_album_title(
            "A long album title that should wrap instead of squeezing the album browser".into(),
        );
        ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
        ui.show().unwrap();
        let pane_width = || {
            ElementHandle::find_by_accessible_label(&ui, "Album browser")
                .next()
                .unwrap()
                .size()
                .width
        };
        assert!((pane_width() - 280.0).abs() < 1.0);
        let roundtrip = |expected: f32| {
            for show_library in [true, false] {
                ui.set_show_library(show_library);
                ui.set_wide(false);
                ui.window().set_size(slint::LogicalSize::new(390.0, 760.0));
                slint::platform::update_timers_and_animations();
                if show_library {
                    assert!((pane_width() - 358.0).abs() < 1.0);
                }
                ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
                ui.set_wide(true);
                slint::platform::update_timers_and_animations();
                assert!(
                    (pane_width() - expected).abs() < 1.0,
                    "actual {} expected {expected}",
                    pane_width()
                );
            }
        };
        roundtrip(280.0);
        let divider = ElementHandle::find_by_accessible_label(&ui, "Resize album browser")
            .next()
            .unwrap();
        let position = divider.absolute_position();
        divider.mock_drag(
            slint::LogicalPosition::new(
                position.x + 88.0,
                position.y + divider.size().height / 2.0,
            ),
            PointerEventButton::Left,
        );
        assert!(
            (pane_width() - 360.0).abs() < 2.0,
            "drag width {}",
            pane_width()
        );
        roundtrip(360.0);
        // A smaller desktop temporarily clamps the pane without erasing its saved width.
        ui.set_library_width(420.0);
        ui.window().set_size(slint::LogicalSize::new(740.0, 680.0));
        slint::platform::update_timers_and_animations();
        assert!(
            (pane_width() - 333.0).abs() < 1.0,
            "clamped width {}",
            pane_width()
        );
        ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
        slint::platform::update_timers_and_animations();
        assert!((pane_width() - 420.0).abs() < 1.0);
    }
    #[test]
    fn desktop_row_selects_double_click_plays_and_hover_button_plays() {
        use slint::platform::{PointerEventButton, WindowEvent};
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = TrackHarness::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(500.0, 48.0));
        let played = Rc::new(Cell::new(0));
        let selected = Rc::new(Cell::new(0));
        let p = played.clone();
        ui.on_play(move || p.set(p.get() + 1));
        let s = selected.clone();
        ui.on_select(move || s.set(s.get() + 1));
        ui.show().unwrap();
        let click = |x| {
            let position = slint::LogicalPosition::new(x, 24.0);
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position });
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        };
        click(150.0);
        assert_eq!(selected.get(), 1);
        assert_eq!(played.get(), 0);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        click(150.0);
        assert_eq!(played.get(), 1);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        click(18.0);
        assert_eq!(played.get(), 2);
    }
    #[test]
    fn mobile_row_plays_on_release_without_a_double_tap() {
        use slint::platform::{PointerEventButton, WindowEvent};
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = TrackHarness::new().unwrap();
        ui.set_tap_to_play(true);
        ui.window().set_size(slint::LogicalSize::new(390.0, 48.0));
        let played = Rc::new(Cell::new(0));
        let count = played.clone();
        ui.on_play(move || count.set(count.get() + 1));
        ui.show().unwrap();
        let position = slint::LogicalPosition::new(150.0, 24.0);
        for expected in 1..=2 {
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            assert_eq!(played.get(), expected - 1, "pressing alone must not play");
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            assert_eq!(
                played.get(),
                expected,
                "one play command per tap, including a double tap"
            );
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        }
    }
    #[test]
    fn mobile_playlist_tap_uses_the_displayed_entry_position() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{PointerEventButton, WindowEvent};
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(390.0, 740.0));
        ui.set_wide(false);
        ui.set_lists_open(true);
        ui.set_list_rows(ModelRc::new(VecModel::from(vec![
            BrowserRow {
                kind: 1,
                title: "Unavailable track".into(),
                status: "Unavailable".into(),
                ..Default::default()
            },
            BrowserRow {
                kind: 1,
                title: "Play from here".into(),
                ..Default::default()
            },
        ])));
        let start = Rc::new(Cell::new(-1));
        let selected = start.clone();
        ui.on_play_list_from(move |index| selected.set(index));
        ui.show().unwrap();
        let row = ElementHandle::find_by_accessible_label(&ui, "Play from here ")
            .next()
            .unwrap();
        let position = slint::LogicalPosition::new(
            row.absolute_position().x + 80.,
            row.absolute_position().y + 20.,
        );
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert_eq!(start.get(), 1);
        assert_eq!(ui.get_list_entry(), 1);
    }
    #[test]
    fn keyboard_transport_and_sort_preserve_album_selection() {
        use slint::platform::WindowEvent;
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.show().unwrap();
        let toggled = Rc::new(Cell::new(0));
        let next = Rc::new(Cell::new(0));
        let previous = Rc::new(Cell::new(0));
        let t = toggled.clone();
        ui.on_toggle_play(move || t.set(t.get() + 1));
        let n = next.clone();
        ui.on_next(move || n.set(n.get() + 1));
        let p = previous.clone();
        ui.on_previous(move || p.set(p.get() + 1));
        for text in [
            SharedString::from(" "),
            slint::platform::Key::RightArrow.into(),
            slint::platform::Key::LeftArrow.into(),
        ] {
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text });
        }
        assert_eq!((toggled.get(), next.get(), previous.get()), (1, 1, 1));
        let dir =
            std::env::temp_dir().join(format!("trash-amp-sort-test-{}.db", std::process::id()));
        let service = TestServices::new(&dir);
        let mut state = State {
            playback: service.playback(),
            artwork: Default::default(),
            browser: browse::Browser::default(),
            library: amp_library::demo_library(),
            selected: Some(0),
            selected_sources: Default::default(),
            catalog: service.catalog(),
            folders: service.catalog().snapshot().folders,
            catalog_revision: 0,
            catalog_replacement: 0,
            preferences: Preferences::default(),
        };
        let id = state.library.albums[0].id.clone();
        sort_albums(&ui, &mut state);
        assert_eq!(state.library.albums[state.selected.unwrap()].id, id);
        assert!(
            state
                .library
                .albums
                .windows(2)
                .all(|w| w[0].title.to_lowercase() <= w[1].title.to_lowercase())
        );
        state.preferences.browse_artists = true;
        sort_albums(&ui, &mut state);
        assert_eq!(state.library.albums[state.selected.unwrap()].id, id);
        assert!(
            state
                .library
                .albums
                .windows(2)
                .all(|w| w[0].title.to_lowercase() <= w[1].title.to_lowercase())
        );
        drop(state);
        let _ = std::fs::remove_file(dir);
    }
    #[test]
    fn playhead_follows_clock_after_user_scrubs_and_restarts() {
        use slint::platform::{PointerEventButton, WindowEvent};
        i_slint_backend_testing::init_no_event_loop();
        let seek = SeekHarness::new().unwrap();
        seek.window().set_size(slint::LogicalSize::new(400.0, 40.0));
        seek.show().unwrap();
        seek.set_position(0.1);
        for target in [0.5, 0.75] {
            let position = slint::LogicalPosition::new(400.0 * target, 20.0);
            seek.window()
                .dispatch_event(WindowEvent::PointerMoved { position });
            seek.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
            seek.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            assert!(
                (seek.get_position() - target).abs() < 0.1,
                "pointer must change the real slider: value={}, target={target}, size={:?}",
                seek.get_position(),
                seek.window().size()
            );
            assert!(!seek.get_scrubbing());
            seek.set_position(0.0);
            assert_eq!(seek.get_displayed_position(), 0.0);
            seek.set_position(0.2);
            assert_eq!(seek.get_displayed_position(), 0.2);
            seek.set_position(0.3);
            assert_eq!(seek.get_displayed_position(), 0.3);
        }
    }
    #[test]
    fn device_source_uses_registered_identity_and_exposes_permission_status() {
        use amp_app::sources::{SourceDescriptor, SourceProvider};
        struct Provider;
        impl SourceProvider for Provider {
            fn descriptors(&self) -> anyhow::Result<Vec<SourceDescriptor>> {
                Ok(vec![
                    SourceDescriptor {
                        id: "test:other".into(),
                        name: "Other music".into(),
                        provider: "test".into(),
                    },
                    SourceDescriptor {
                        id: "apple-library:device".into(),
                        name: "Device Music Library".into(),
                        provider: "apple-library".into(),
                    },
                ])
            }
            fn load(
                &self,
                id: &str,
                _: bool,
                _: &dyn Fn() -> bool,
            ) -> anyhow::Result<amp_app::catalog::ExternalCatalog> {
                anyhow::bail!("{id}: Music library access denied")
            }
        }
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile_test_directory();
        let service = TestServices::new(&directory.join("library.sqlite3"));
        let catalog = service.catalog();
        catalog.register_sources(vec![Arc::new(Provider)], false);
        let initial = catalog.bootstrap_snapshot().unwrap();
        assert_eq!(
            initial.sources.len(),
            2,
            "No implicit folder row on a host without folder access"
        );
        let state = Rc::new(RefCell::new(State {
            catalog: catalog.clone(),
            playback: service.playback(),
            folders: initial.folders,
            catalog_revision: 0,
            catalog_replacement: 0,
            library: Library::default(),
            artwork: Default::default(),
            browser: Default::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: initial.preferences,
        }));
        let ui = PlayerWindow::new().unwrap();
        ui.set_home(true);
        ui.window().set_size(slint::LogicalSize::new(390., 844.));
        wire_source_selection(&ui, &state);
        ui.invoke_source_changed(1);
        assert!(ui.get_device_library_active());
        assert!(!ui.get_collector_active());
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while catalog
            .bootstrap_snapshot()
            .unwrap()
            .source_message
            .as_ref()
            .is_none_or(|m| !m.contains("access denied"))
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        render_catalog(&ui, &mut state.borrow_mut(), &mut 0);
        assert!(
            ui.get_source_status()
                .contains("apple-library:device: Music library access denied")
        );
        assert!(ui.get_device_library_available());
        assert_eq!(
            catalog.snapshot().source,
            CatalogSource::Local,
            "Failed access preserves the accepted source"
        );
        ui.show().unwrap();
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                &ui,
                "Music source status"
            )
            .next()
            .is_some()
        );
        drop(state);
        drop(service);
        std::fs::remove_dir_all(directory).unwrap();
    }
    fn tempfile_test_directory() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("trash-amp-source-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
    #[test]
    fn native_catalog_refresh_preserves_track_identity_and_clears_a_removed_album() {
        use slint::Model;
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile_test_directory();
        let services = TestServices::new(&directory.join("library.sqlite3"));
        let catalog = services.catalog();
        let initial = catalog.snapshot();
        let state = Rc::new(RefCell::new(State {
            catalog: catalog.clone(),
            playback: services.playback(),
            folders: initial.folders,
            catalog_revision: initial.library_revision,
            catalog_replacement: initial.replacement_revision,
            library: Library::default(),
            artwork: Default::default(),
            browser: Default::default(),
            selected: None,
            selected_sources: Default::default(),
            preferences: initial.preferences,
        }));
        let ui = PlayerWindow::new().unwrap();
        let mut notice = 0;
        let publish = |library: Library| {
            let version = catalog.generation().advance();
            assert!(
                catalog
                    .complete_external(
                        version,
                        amp_app::catalog::ExternalCatalog {
                            online: None,
                            source: "native:test".into(),
                            library,
                            artwork: Default::default()
                        },
                        Arc::new(|| Ok(Box::new(|| Ok(()))))
                    )
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap()
            );
        };
        let mut library = amp_library::demo_library();
        library.albums.truncate(1);
        library.albums[0].tracks.truncate(3);
        publish(library.clone());
        render_catalog(&ui, &mut state.borrow_mut(), &mut notice);
        select(&ui, &mut state.borrow_mut(), 0);
        ui.set_selected_track(1);
        let selected = library.albums[0].tracks[1].id.clone();
        library.albums[0].tracks.remove(0);
        publish(library.clone());
        render_catalog(&ui, &mut state.borrow_mut(), &mut notice);
        assert_eq!(ui.get_selected_track(), 0);
        assert_eq!(state.borrow().library.albums[0].tracks[0].id, selected);
        library.albums.clear();
        publish(library);
        render_catalog(&ui, &mut state.borrow_mut(), &mut notice);
        assert!(state.borrow().selected.is_none());
        assert_eq!(ui.get_tracks().row_count(), 0);
        drop(state);
        drop(services);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
