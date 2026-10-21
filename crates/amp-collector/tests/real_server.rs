//! Real TLS listener, archive publication, native authorization and FLAC pipeline.
#[cfg(test)]
use amp_collector::{Adapter, ListCommand, api};
use amp_collector::{ConnectionKey, ids::*};
use collector_app::{AuthenticatedPrincipal, CommandContext, CredentialKind};
use collector_client::{
    Endpoint, EnrolledCredential, NativeCredentialStorage, Pairing, PendingEnrollment, Route,
};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Secrets(Mutex<Vec<EnrolledCredential>>);
impl NativeCredentialStorage for Secrets {
    fn save(&self, c: &EnrolledCredential) -> Result<()> {
        self.0.lock().unwrap().push(c.clone());
        Ok(())
    }
    fn load(&self, k: ConnectionKey) -> Result<Option<EnrolledCredential>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|c| c.server_id == k.server_id && c.user_id == Some(k.user_id))
            .cloned())
    }
    fn remove(&self, k: ConnectionKey) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .retain(|c| c.server_id != k.server_id || c.user_id != Some(k.user_id));
        Ok(())
    }
    fn load_legacy(&self, _: ServerId) -> Result<Option<EnrolledCredential>> {
        Ok(None)
    }
    fn remove_legacy(&self, _: ServerId) -> Result<()> {
        Ok(())
    }
}
fn command() -> CommandContext {
    CommandContext::new(uuid::Uuid::new_v4().to_string()).unwrap()
}
pub struct Fixture {
    _root: tempfile::TempDir,
    _state: collector_server::AppState,
    pub pairing: Pairing,
    pub owner: Principal,
    stop: axum_server::Handle<std::net::SocketAddr>,
    task: tokio::task::JoinHandle<()>,
    lose_list_response: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.shutdown();
        self.task.abort();
    }
}
impl Fixture {
    pub async fn new(item_id: ItemId, label: &str, game: bool) -> Self {
        let module = if game {
            "org.junk.games"
        } else {
            "org.junk.music"
        };
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(
            collector_store::Store::open(root.path().join("archive"), root.path().join("state"))
                .unwrap(),
        );
        let owner = Principal {
            user_id: store
                .create_user("owner", "long test password", true)
                .unwrap(),
            administrator: true,
            device_id: None,
        };
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .unwrap();
        let pem = cert.cert.pem();
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(
            pem.clone().into_bytes(),
            cert.key_pair.serialize_pem().into_bytes(),
        )
        .await
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("https://{}", listener.local_addr().unwrap());
        let pairing = Pairing {
            server_id: store.server_id().unwrap(),
            certificate_sha256: Digest::sha256(pem.as_bytes()).hex,
            certificate_pem: pem,
            endpoints: vec![Endpoint {
                url: base.parse().unwrap(),
                route: Route::Lan,
                latency_ms: None,
                healthy: true,
            }],
        };
        let mut state = collector_server::AppState::new(store, pairing.clone());
        state.ingress = Ingress::Private;
        state.allowed_origins = Arc::new(vec![base.clone()]);
        let auth = AuthenticatedPrincipal {
            principal: owner.clone(),
            credential_kind: CredentialKind::TestLocal,
            authenticated_at: chrono::Utc::now(),
        };
        let collection = state
            .service
            .create_collection(
                &auth,
                collector_app::CreateCollectionCommand {
                    name: label.into(),
                    module: ModuleId(module.into()),
                },
                &command(),
            )
            .unwrap();
        let item = Item {
            lifecycle: Default::default(),
            id: item_id,
            collection_id: collection.id,
            revision: Revision(0),
            title: label.into(),
            subtitle: "Fixture artist".into(),
            identity_link: ItemIdentityLink::None,
            domain: ModuleData {
                module: ModuleId(module.into()),
                schema_version: 1,
                payload: RawJson::from_value(if game {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"tracks":[{"position":1,"title":"Song","start":0,"length":176400}]})
                }),
            },
            identity: IdentityState::Unidentified,
            translations: vec![],
            identifiers: vec![],
            specimens: vec![],
            wishlist: false,
            captures: vec![],
            assets: vec![],
            metadata: Default::default(),
            deleted: false,
            updated_at: chrono::Utc::now(),
        };
        let item = state
            .service
            .save_item(&owner, item_id, item.into(), Revision(0), &command())
            .unwrap();
        if game {
            state
                .service
                .create_asset(
                    &owner,
                    collector_app::CreateAssetCommand {
                        collection_id: collection.id,
                        subject: AssetSubject::Item { item_id },
                        kind: AssetKind::Cover,
                        visibility: Visibility::Shared,
                        alt_text: "Game artwork".into(),
                    },
                    include_bytes!("fixtures/digital-audio/cover.png"),
                    &command(),
                )
                .unwrap();
        }
        let cue = b"FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n INDEX 01 00:00:00\n";
        let cue: &[u8] = if game {
            b"FILE \"disc.bin\" BINARY\n TRACK 01 MODE2/2352\n INDEX 01 00:00:00\n TRACK 02 AUDIO\n INDEX 01 00:00:01\n"
        } else {
            cue
        };
        let pcm = (0..(176400 + if game { 588 } else { 0 }))
            .flat_map(|frame| {
                let _ = frame;
                let n = 0i16.to_le_bytes();
                [n[0], n[1], n[0], n[1]]
            })
            .collect::<Vec<_>>();
        let import = state
            .service
            .create_provisional_import(
                &owner,
                collector_app::CreateImportCommand {
                    collection_id: collection.id,
                    source: collector_app::ImportSourceSummary {
                        kind: collector_app::ImportSourceKind::Browser,
                        label: None,
                        package_name: "fixture".into(),
                    },
                    files: vec![
                        collector_app::UploadFile {
                            visibility: Default::default(),
                            name: "disc.cue".into(),
                            bytes: ByteLength(cue.len() as u64),
                            sha256: None,
                        },
                        collector_app::UploadFile {
                            visibility: Default::default(),
                            name: "disc.bin".into(),
                            bytes: ByteLength(pcm.len() as u64),
                            sha256: None,
                        },
                    ],
                    association: CaptureAssociation {
                        item_id: Some(item_id),
                        ..Default::default()
                    },
                    expected_item_revision: Some(item.revision),
                },
                &command(),
            )
            .unwrap();
        state
            .service
            .stage_provisional_import_chunk(&owner, import.id, 0, 0, cue)
            .unwrap();
        state
            .service
            .stage_provisional_import_chunk(&owner, import.id, 1, 0, &pcm)
            .unwrap();
        state
            .service
            .enqueue_import_analysis(&owner, import.id, ModuleId(module.into()), &command())
            .unwrap();
        state.service.run_next_job("fixture-analysis").unwrap();
        let analyzed = state.service.provisional_import(&owner, import.id).unwrap();
        let reviewed = state
            .service
            .review_provisional_import(
                &owner,
                import.id,
                collector_app::ReviewImportCommand {
                    expected_revision: analyzed.revision,
                    identity: collector_app::ImportIdentityDecision::Unidentified,
                    association: CaptureAssociation {
                        item_id: Some(item_id),
                        ..Default::default()
                    },
                    expected_item_revision: Some(item.revision),
                },
                &command(),
            )
            .unwrap();
        state
            .service
            .enqueue_import_publication(&owner, reviewed.id, &command())
            .unwrap();
        state.service.run_next_job("fixture-publication").unwrap();
        let row = state
            .service
            .music_track_availability(&owner, item_id)
            .unwrap()
            .into_iter()
            .find(|r| r.primary_playback())
            .unwrap();
        let tool = collector_providers::ConfiguredEncoder::probe(
            collector_providers::EncoderKind::Flac,
            "flac",
        )
        .expect("real-server acceptance requires flac");
        state.service = state.service.clone().with_audio_converter(Arc::new(
            collector_providers::ProcessAudioConverter::new(Some(tool), None).unwrap(),
        ));
        state
            .service
            .prepare_configured_music_track(
                &owner,
                item_id,
                row.capture_id,
                collector_app::PrepareMusicTrack {
                    source_representation_id: row.source_representation_id,
                    profile: ConversionProfile::Flac,
                },
                row.track_position,
                &command(),
            )
            .unwrap();
        state.service.run_next_job("fixture-flac").unwrap();
        let stop = axum_server::Handle::new();
        let handle = stop.clone();
        let lose_list_response = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let response_fault = lose_list_response.clone();
        let app = collector_server::router(state.clone()).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let fault = response_fault.clone();
                async move {
                    let list_write = matches!(
                        *request.method(),
                        axum::http::Method::POST | axum::http::Method::PUT
                    ) && request.uri().path().contains("/lists");
                    let mut response = next.run(request).await;
                    if list_write
                        && response.status().is_success()
                        && fault.swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        // The actual effect has committed. Only delivery is damaged.
                        response
                            .headers_mut()
                            .remove(axum::http::header::CONTENT_LENGTH);
                        *response.body_mut() = axum::body::Body::from("{");
                    }
                    response
                }
            },
        ));
        let task = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, config)
                .unwrap()
                .handle(handle)
                .serve(app.into_make_service())
                .await
                .unwrap();
        });
        Self {
            _root: root,
            _state: state,
            pairing,
            owner,
            stop,
            task,
            lose_list_response,
        }
    }
    async fn enroll(&self, secrets: &Secrets) -> ConnectionKey {
        self.enroll_as(secrets, "owner", "long test password").await
    }
    async fn enroll_as(&self, secrets: &Secrets, username: &str, password: &str) -> ConnectionKey {
        let client = self.pairing.lan_client().unwrap();
        let base = self.pairing.endpoints[0].url.to_string();
        let mut pending =
            PendingEnrollment::start(&client, base.parse().unwrap(), "Trash Amp acceptance")
                .await
                .unwrap();
        pending.pin(self.pairing.clone()).unwrap();
        let login=client.post(format!("{base}api/v1/auth/login")).header("origin",base.trim_end_matches('/')).json(&serde_json::json!({"username":username,"password":password,"device_name":"fixture"})).send().await.unwrap();
        assert!(login.status().is_success());
        let cookie = login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let approved = client
            .post(format!("{base}api/v1/enrollments/approve"))
            .header("origin", base.trim_end_matches('/'))
            .header("cookie", cookie)
            .json(&serde_json::json!({"user_code":pending.start.user_code}))
            .send()
            .await
            .unwrap();
        assert!(approved.status().is_success());
        let credential = pending.complete(&client).await.unwrap();
        let key = ConnectionKey {
            server_id: credential.server_id,
            user_id: credential.user_id.unwrap(),
        };
        secrets.save(&credential).unwrap();
        key
    }
}
impl Fixture {
    pub fn add_digital_purchase(&self) {
        let flac = include_bytes!("fixtures/digital-audio/01.flac").as_slice();
        let mp3 = include_bytes!("fixtures/digital-audio/02.mp3").as_slice();
        let cover = include_bytes!("fixtures/digital-audio/cover.png").as_slice();
        self.add_digital_purchase_files(flac, mp3, cover);
    }
    pub fn add_digital_purchase_files(&self, flac: &[u8], mp3: &[u8], cover: &[u8]) {
        let service = &self._state.service;
        let collection = service
            .create_collection(
                &AuthenticatedPrincipal {
                    principal: self.owner.clone(),
                    credential_kind: CredentialKind::TestLocal,
                    authenticated_at: chrono::Utc::now(),
                },
                collector_app::CreateCollectionCommand {
                    name: "Digital purchases".into(),
                    module: ModuleId("org.junk.music".into()),
                },
                &command(),
            )
            .unwrap();
        let plan = service
            .plan_ingest(
                &self.owner,
                collector_app::PlanIngestCommand {
                    collection_id: collection.id,
                    packages: vec![collector_app::IngestPackageSelection {
                        purchase: Some(DigitalPurchase {
                            store: Some("Private store".into()),
                            ..Default::default()
                        }),
                        source: collector_app::ImportSourceSummary {
                            kind: collector_app::ImportSourceKind::Browser,
                            label: None,
                            package_name: "Digital native".into(),
                        },
                        anchor_file_index: None,
                        files: [("01.flac", flac), ("02.mp3", mp3), ("cover.png", cover)]
                            .into_iter()
                            .map(|(name, bytes)| collector_app::UploadFile {
                                visibility: Some(Visibility::Shared),
                                name: name.into(),
                                bytes: ByteLength(bytes.len() as u64),
                                sha256: None,
                            })
                            .collect(),
                    }],
                },
            )
            .unwrap();
        let accepted = service
            .start_ingest(
                &self.owner,
                collector_app::StartIngestCommand {
                    purchase: None,
                    clean_reimport: None,
                    plan,
                    analysis: collector_app::IngestAnalysisPolicy::Automatic,
                    enrichment: collector_app::IngestEnrichmentPolicy::None,
                    prepare_music: false,
                    destination: None,
                },
                &command(),
            )
            .unwrap();
        let import = accepted.imports[0].import_id;
        for (index, bytes) in [flac, mp3, cover].into_iter().enumerate() {
            service
                .stage_provisional_import_chunk(&self.owner, import, index, 0, bytes)
                .unwrap();
        }
        for _ in 0..10 {
            if !service.run_next_job("digital-native").unwrap() {
                break;
            }
        }
        service.resume_guided_ingests().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_real_servers_stream_download_edit_and_retain_offline_audio() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let item = ItemId::new();
    let first = Fixture::new(item, "First album", false).await;
    let second = Fixture::new(item, "Second album", false).await;
    let secrets = Arc::new(Secrets::default());
    let one = first.enroll(&secrets).await;
    let two = second.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    let a = adapter.refresh(one, &|| false).await.unwrap();
    let b = adapter.refresh(two, &|| false).await.unwrap();
    assert!(Adapter::with_credentials(cache.path(), secrets.clone()).is_err());
    assert!(adapter.refresh(one, &|| true).await.is_err());
    assert_eq!(adapter.snapshot(one).unwrap().unwrap().updated, a.updated);
    assert_eq!(a.library.albums[0].id.item, b.library.albums[0].id.item);
    assert_ne!(
        a.library.albums[0].id.library,
        b.library.albums[0].id.library
    );
    assert_eq!(
        adapter.snapshot(one).unwrap().unwrap().library.albums[0].title,
        "First album"
    );
    let collection = a.collections[0].id;
    let created = adapter
        .submit(ListCommand {
            key: one,
            collection,
            list: None,
            create: Some(api::CreateCollectionList {
                kind: Some(CollectionListKind::MusicTracks),
                name: "Road music".into(),
            }),
            update: None,
            receipt: "create-list".into(),
        })
        .await
        .unwrap();
    let remote: &amp_collector::RemoteTrack =
        &serde_json::from_str(match &a.library.albums[0].tracks[0].access {
            amp_core::MediaAccess::Managed { item, .. } => item,
            _ => panic!(),
        })
        .unwrap();
    for revision in [created.revision, Revision(created.revision.0 + 1)] {
        adapter
            .submit(ListCommand {
                key: one,
                collection,
                list: Some(created.id),
                create: None,
                update: Some(api::UpdateCollectionList {
                    expected_revision: revision,
                    change: api::CollectionListChange::Append {
                        item_id: item,
                        source_representation_id: Some(remote.source),
                        track_position: Some(1),
                    },
                }),
                receipt: format!("append-{}", revision.0),
            })
            .await
            .unwrap();
    }
    let snapshot = adapter.refresh(one, &|| false).await.unwrap();
    assert_eq!(snapshot.lists[0].entries.items.len(), 2);
    assert_ne!(
        snapshot.lists[0].entries.items[0].id,
        snapshot.lists[0].entries.items[1].id
    );
    assert!(snapshot.lists[0].entries.items[0].source.is_some());
    assert!(snapshot.lists[0].entries.items[0].audio.is_some());
    let track = snapshot.library.albums[0].tracks[0].clone();
    let a = adapter.clone();
    let t = track.clone();
    tokio::task::spawn_blocking(move || {
        use amp_audio::MediaResolver;
        let source = a.resolve(&t).unwrap();
        let mut decoder = junk_libs_audio::AudioDecoder::open(source).unwrap();
        let mut frames = 0;
        while let Some(chunk) = decoder.read_frames(1024).unwrap() {
            frames += chunk.frame_count()
        }
        assert_eq!(frames, 176400);
        a.schedule(std::slice::from_ref(&t)).unwrap();
        a.download(t).unwrap();
    })
    .await
    .unwrap();
    let mut historical = snapshot.lists[0].clone();
    for entry in &mut historical.entries.items {
        entry.audio = None;
    }
    let retained = adapter.list_tracks(one, &historical);
    assert_eq!(retained.len(), 2);
    assert!(retained.iter().all(|track| adapter.downloaded(track)));
    first.stop.shutdown();
    second.stop.shutdown();
    secrets.remove(one).unwrap();
    assert_ne!(first.owner.user_id, second.owner.user_id);
    let a = adapter.clone();
    tokio::task::spawn_blocking(move || {
        use amp_audio::MediaResolver;
        let source = a.resolve(&track).unwrap();
        let mut decoder = junk_libs_audio::AudioDecoder::open(source).unwrap();
        decoder
            .seek_exact(junk_libs_audio::FramePosition(44100))
            .unwrap();
        assert!(decoder.read_frames(1024).unwrap().is_some());
    })
    .await
    .unwrap();
    assert_eq!(adapter.downloads().unwrap()[0].status, "complete");
}

#[tokio::test(flavor = "multi_thread")]
async fn digital_flac_mp3_stream_seek_download_and_offline_reopen() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let fixture = Fixture::new(ItemId::new(), "CD control", false).await;
    fixture.add_digital_purchase();
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    let snapshot = adapter.refresh(key, &|| false).await.unwrap();
    let album = snapshot
        .library
        .albums
        .iter()
        .find(|album| album.title == "Purchased album")
        .expect("purchased album in native library");
    assert_eq!(album.tracks.len(), 2);
    assert!(
        !snapshot.artwork.is_empty(),
        "purchase artwork retained in native snapshot"
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("Private store")
    );
    let tracks = album.tracks.clone();
    assert_ne!(tracks[0].id, tracks[1].id);
    let a = adapter.clone();
    let selected = tracks.clone();
    tokio::task::spawn_blocking(move || {
        use amp_audio::MediaResolver;
        for (index, track) in selected.iter().enumerate() {
            let mut decoder =
                junk_libs_audio::AudioDecoder::open(a.resolve(track).unwrap()).unwrap();
            assert_eq!(
                decoder.format().sample_rate(),
                if index == 0 { 96000 } else { 44100 }
            );
            decoder
                .seek_exact(junk_libs_audio::FramePosition(100))
                .unwrap();
            assert_eq!(
                decoder.read_frames(128).unwrap().unwrap().start_frame,
                junk_libs_audio::FramePosition(100)
            );
        }
        a.schedule(&selected).unwrap();
        for track in selected {
            a.download(track).unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        adapter
            .downloads()
            .unwrap()
            .iter()
            .filter(|d| d.status == "complete")
            .count(),
        2
    );
    fixture.stop.shutdown();
    secrets.remove(key).unwrap();
    drop(adapter);
    let adapter = Adapter::with_credentials(cache.path(), secrets).unwrap();
    assert_eq!(
        adapter
            .snapshot(key)
            .unwrap()
            .unwrap()
            .library
            .albums
            .iter()
            .find(|album| album.title == "Purchased album")
            .unwrap()
            .tracks
            .len(),
        2
    );
    tokio::task::spawn_blocking(move || {
        use amp_audio::MediaResolver;
        for track in tracks {
            let mut decoder =
                junk_libs_audio::AudioDecoder::open(adapter.resolve(&track).unwrap()).unwrap();
            decoder
                .seek_exact(junk_libs_audio::FramePosition(100))
                .unwrap();
            assert!(decoder.read_frames(128).unwrap().is_some());
        }
    })
    .await
    .unwrap();
}

/// Manual qualification uses the actual default output device and the ordinary
/// decoder/resampler/ring-buffer callback. Ordinary CI never requires a device.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the host audio output device"]
async fn purchased_originals_reach_the_real_audio_output_callback() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let fixture = Fixture::new(ItemId::new(), "CD control", false).await;
    fixture.add_digital_purchase();
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets).unwrap();
    let snapshot = adapter.refresh(key, &|| false).await.unwrap();
    let tracks = snapshot
        .library
        .albums
        .iter()
        .find(|a| a.title == "Purchased album")
        .unwrap()
        .tracks
        .clone();
    tokio::task::spawn_blocking(move || {
        for track in tracks {
            let player = amp_audio::Player::with_resolver(Some(adapter.clone())).unwrap();
            let label = track.title.clone();
            player.play(vec![track], 0);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut peak = 0.0f32;
            let mut position = 0.0f64;
            while std::time::Instant::now() < deadline {
                if let Some(error) = player.error() {
                    panic!("{label}: {error}");
                }
                let snapshot = player.snapshot();
                peak = peak.max(snapshot.level);
                position = position.max(snapshot.seconds);
                if peak > 0.0001 && position > 0.02 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                peak > 0.0001 && position > 0.02,
                "{label}: device callback peak={peak}, seconds={position}"
            );
            println!("{label}: actual device callback peak={peak}, position={position}");
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn game_disc_audio_streams_seeks_and_reopens_offline() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let item = ItemId::new();
    let fixture = Fixture::new(item, "Game soundtrack", true).await;
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    let snapshot = adapter.refresh(key, &|| false).await.unwrap();
    assert_eq!(snapshot.library.albums.len(), 1);
    assert_eq!(snapshot.artwork.len(), 1);
    assert!(
        snapshot
            .artwork
            .values()
            .all(|path| std::path::Path::new(path).is_file())
    );
    let album = &snapshot.library.albums[0];
    assert_eq!(album.title, "Game soundtrack");
    assert_eq!(album.tracks.len(), 1);
    let track = album.tracks[0].clone();
    assert_eq!(track.title, "Track 02");
    let a = adapter.clone();
    let t = track.clone();
    tokio::task::spawn_blocking(move || {
        use amp_audio::MediaResolver;
        let mut decoder = junk_libs_audio::AudioDecoder::open(a.resolve(&t).unwrap()).unwrap();
        let mut frames = 0;
        while let Some(chunk) = decoder.read_frames(1024).unwrap() {
            frames += chunk.frame_count();
        }
        assert_eq!(frames, 176400);
        decoder
            .seek_exact(junk_libs_audio::FramePosition(44100))
            .unwrap();
        assert!(decoder.read_frames(1024).unwrap().is_some());
        a.schedule(std::slice::from_ref(&t)).unwrap();
        a.download(t).unwrap();
    })
    .await
    .unwrap();
    fixture.stop.shutdown();
    secrets.remove(key).unwrap();
    drop(adapter);
    let adapter = Adapter::with_credentials(cache.path(), secrets).unwrap();
    assert!(adapter.downloaded(&track));
    let offline = adapter.snapshot(key).unwrap().unwrap();
    assert_eq!(offline.library.albums[0].title, "Game soundtrack");
    assert_eq!(offline.artwork.len(), 1);
    assert!(
        offline
            .artwork
            .values()
            .all(|path| std::path::Path::new(path).is_file())
    );
    tokio::task::spawn_blocking(move || {
        use amp_audio::MediaResolver;
        let mut decoder =
            junk_libs_audio::AudioDecoder::open(adapter.resolve(&track).unwrap()).unwrap();
        decoder
            .seek_exact(junk_libs_audio::FramePosition(88200))
            .unwrap();
        assert!(decoder.read_frames(1024).unwrap().is_some());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_list_response_loss_restarts_with_exact_input_and_no_duplicate_entries() {
    use std::sync::atomic::Ordering;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let item = ItemId::new();
    let fixture = Fixture::new(item, "Durable list control", false).await;
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    let snapshot = adapter.refresh(key, &|| false).await.unwrap();
    let collection = snapshot.collections[0].id;
    let create = ListCommand {
        key,
        collection,
        list: None,
        create: Some(api::CreateCollectionList {
            kind: Some(CollectionListKind::MusicTracks),
            name: "Original request".into(),
        }),
        update: None,
        receipt: "uncertain-create".into(),
    };
    fixture.lose_list_response.store(true, Ordering::SeqCst);
    assert!(adapter.submit(create.clone()).await.is_err());
    let client = adapter.client(key).unwrap();
    let lists = client
        .lists(
            collection,
            &api::CollectionListsQuery {
                search: None,
                cursor: None,
                limit: Some(100),
            },
        )
        .await
        .unwrap();
    assert_eq!(lists.items.len(), 1);
    let committed = lists.items[0].clone();
    let mut changed = create.clone();
    changed.create.as_mut().unwrap().name = "Changed request".into();
    assert!(adapter.submit(changed).await.is_err());
    let mut other_server = create.clone();
    other_server.key.server_id = ServerId::new();
    assert!(adapter.submit(other_server).await.is_err());
    let pending = adapter.pending_commands(key).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        serde_json::to_value(&pending[0]).unwrap(),
        serde_json::to_value(&create).unwrap()
    );
    drop(adapter);
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    let retried = adapter
        .submit(adapter.pending_commands(key).unwrap().pop().unwrap())
        .await
        .unwrap();
    assert_eq!(retried.id, committed.id);
    assert_eq!(retried.revision, committed.revision);
    assert!(adapter.pending_commands(key).unwrap().is_empty());
    let remote: amp_collector::RemoteTrack =
        serde_json::from_str(match &snapshot.library.albums[0].tracks[0].access {
            amp_core::MediaAccess::Managed { item, .. } => item,
            _ => panic!(),
        })
        .unwrap();
    let append = ListCommand {
        key,
        collection,
        list: Some(committed.id),
        create: None,
        update: Some(api::UpdateCollectionList {
            expected_revision: committed.revision,
            change: api::CollectionListChange::Append {
                item_id: item,
                source_representation_id: Some(remote.source),
                track_position: Some(1),
            },
        }),
        receipt: "uncertain-append".into(),
    };
    fixture.lose_list_response.store(true, Ordering::SeqCst);
    assert!(adapter.submit(append.clone()).await.is_err());
    let committed_list = client
        .list_snapshot(collection, committed.id)
        .await
        .unwrap();
    assert_eq!(committed_list.entries.items.len(), 1);
    let entry = committed_list.entries.items[0].id;
    let mut changed = append.clone();
    changed.update.as_mut().unwrap().expected_revision = committed_list.list.revision;
    assert!(adapter.submit(changed).await.is_err());
    drop(adapter);
    let adapter = Adapter::with_credentials(cache.path(), secrets).unwrap();
    let replay = adapter
        .submit(adapter.pending_commands(key).unwrap().pop().unwrap())
        .await
        .unwrap();
    assert_eq!(replay.revision, committed_list.list.revision);
    assert!(adapter.pending_commands(key).unwrap().is_empty());
    let final_list = client
        .list_snapshot(collection, committed.id)
        .await
        .unwrap();
    assert_eq!(final_list.entries.items.len(), 1);
    assert_eq!(final_list.entries.items[0].id, entry);
    let final_lists = client
        .lists(
            collection,
            &api::CollectionListsQuery {
                search: None,
                cursor: None,
                limit: Some(100),
            },
        )
        .await
        .unwrap();
    assert_eq!(final_lists.items.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_and_native_credential_revocation_deny_the_next_actual_uncached_read() {
    use std::{
        io::{ErrorKind, Read},
        time::Duration,
    };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    for device_revocation in [false, true] {
        let item = ItemId::new();
        let fixture = Fixture::new(item, "Native read control", false).await;
        let service = &fixture._state.service;
        let store = &fixture._state.store;
        let record = collector_app::Catalog::item(store.as_ref(), item).unwrap();
        let master = record
            .captures
            .iter()
            .flat_map(|capture| &capture.representations)
            .find(|representation| representation.role == RepresentationRole::PreservationMaster)
            .unwrap();
        let file_index = master
            .files
            .iter()
            .position(|file| file.name == "disc.bin")
            .unwrap();
        assert!(master.files[file_index].bytes.0 > 256 * 1024);
        let source = master.id;
        let collection = record.collection_id;
        let user = store
            .create_user("native-reader", "long reader password", false)
            .unwrap();
        collector_app::Catalog::grant(store.as_ref(), collection, user, Role::Viewer).unwrap();
        let secrets = Arc::new(Secrets::default());
        let key = fixture
            .enroll_as(&secrets, "native-reader", "long reader password")
            .await;
        let client = collector_client::EnrolledClient::from_storage(secrets.as_ref(), key).unwrap();
        let cancellation = collector_client::MediaCancellation::default();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let read = tokio::task::spawn_blocking(move || {
            let mut reader = client
                .media_reader(item, source, file_index, cancellation)
                .unwrap();
            let mut first = vec![0u8; 256 * 1024];
            reader.read_exact(&mut first).unwrap();
            ready_tx.send(()).unwrap();
            continue_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let error = reader.read(&mut [0u8; 1]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        });
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        if device_revocation {
            let authenticated = store
                .login("native-reader", "long reader password", "revoke control")
                .unwrap()
                .1;
            let devices = service.native_devices(&authenticated).unwrap();
            assert_eq!(devices.len(), 1);
            service
                .revoke_native_device(
                    &authenticated,
                    devices[0].id,
                    devices[0].revision,
                    &command(),
                )
                .unwrap();
        } else {
            let authenticated = store
                .login("owner", "long test password", "revoke control")
                .unwrap()
                .1;
            let grant = service
                .grant_for_username(&fixture.owner, collection, "native-reader")
                .unwrap()
                .unwrap();
            service
                .set_grant(
                    &authenticated,
                    collection,
                    "native-reader",
                    grant.revision,
                    None,
                    &command(),
                )
                .unwrap();
        }
        continue_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap();
    }
}
