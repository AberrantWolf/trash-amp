//! Real TLS listener, archive publication, native authorization and FLAC pipeline.
#[cfg(test)]
use amp_collector::{Adapter, ConnectionState, ListCommand, api};
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
    _local: Option<collector_server::local_playback::Runtime>,
    _remote: Option<RemoteFixture>,
    _state: collector_server::AppState,
    pub pairing: Pairing,
    pub owner: Principal,
    stop: axum_server::Handle<std::net::SocketAddr>,
    task: tokio::task::JoinHandle<()>,
}
struct RemoteFixture {
    _authority: collector_server::remote_media::Runtime,
    task: tokio::task::JoinHandle<()>,
    certificate: String,
}
impl Drop for RemoteFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.shutdown();
        self.task.abort();
    }
}
impl Fixture {
    pub async fn new(item_id: ItemId, label: &str) -> Self {
        Self::with_media_policy(item_id, label, true, false).await
    }
    async fn with_media_policy(item_id: ItemId, label: &str, local: bool, public: bool) -> Self {
        Self::with_routes(item_id, label, local, public, false).await
    }
    async fn with_routes(
        item_id: ItemId,
        label: &str,
        local: bool,
        public: bool,
        remote: bool,
    ) -> Self {
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
                    module: ModuleId("org.junk.music".into()),
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
                module: ModuleId("org.junk.music".into()),
                schema_version: 1,
                payload: RawJson::from_value(
                    serde_json::json!({"tracks":[{"position":1,"title":"Song","start":0,"length":176400}]}),
                ),
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
        let cue = b"FILE \"disc.bin\" BINARY\n TRACK 01 AUDIO\n INDEX 01 00:00:00\n";
        let pcm = (0..176400)
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
            .enqueue_import_analysis(
                &owner,
                import.id,
                ModuleId("org.junk.music".into()),
                &command(),
            )
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
                1,
                &command(),
            )
            .unwrap();
        state.service.run_next_job("fixture-flac").unwrap();
        let stop = axum_server::Handle::new();
        let handle = stop.clone();
        let local = if local {
            Some(
                collector_server::local_playback::start(&mut state, "127.0.0.1:0".parse().unwrap())
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        state.direct_media = public;
        state.remote_media_allowed = public;
        let remote = if remote {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let certificate = cert.cert.pem();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
                certificate.clone().into_bytes(),
                cert.key_pair.serialize_pem().into_bytes(),
            )
            .await
            .unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let config = collector_server::remote_media::Configuration {
                base_url: format!(
                    "https://localhost:{}/",
                    listener.local_addr().unwrap().port()
                ),
                socket: root.path().join("authority/socket"),
            };
            state.remote_media = Some(Arc::new(config.clone()));
            let authority = collector_server::remote_media::start(state.clone(), &config)
                .await
                .unwrap();
            let key = std::fs::read_to_string(config.socket.parent().unwrap().join("service-key"))
                .unwrap();
            let media = collector_media::MediaState::new(
                &root.path().join("archive"),
                &config.socket,
                key,
                vec![],
            )
            .unwrap();
            let task = tokio::spawn(async move {
                axum_server::from_tcp_rustls(listener, tls)
                    .unwrap()
                    .serve(collector_media::router(media).into_make_service())
                    .await
                    .unwrap();
            });
            Some(RemoteFixture {
                _authority: authority,
                task,
                certificate,
            })
        } else {
            None
        };
        let app = collector_server::router(state.clone());
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
            _local: local,
            _remote: remote,
            _state: state,
            pairing,
            owner,
            stop,
            task,
        }
    }
    async fn enroll(&self, secrets: &Secrets) -> ConnectionKey {
        let client = self.pairing.lan_client().unwrap();
        let base = self.pairing.endpoints[0].url.to_string();
        let mut pending =
            PendingEnrollment::start(&client, base.parse().unwrap(), "Trash Amp acceptance")
                .await
                .unwrap();
        pending.pin(self.pairing.clone()).unwrap();
        let login=client.post(format!("{base}api/v1/auth/login")).header("origin",base.trim_end_matches('/')).json(&serde_json::json!({"username":"owner","password":"long test password","device_name":"fixture"})).send().await.unwrap();
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
    let first = Fixture::new(item, "First album").await;
    let second = Fixture::new(item, "Second album").await;
    let secrets = Arc::new(Secrets::default());
    let one = first.enroll(&secrets).await;
    let two = second.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    let a = adapter.refresh(one, &|| false).await.unwrap();
    let b = adapter.refresh(two, &|| false).await.unwrap();
    let probe = adapter.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || probe.connection_state(one))
            .await
            .unwrap(),
        ConnectionState::Lan
    );
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
                kind: None,
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
    let probe = adapter.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || probe.connection_state(one))
            .await
            .unwrap(),
        ConnectionState::Disconnected
    );
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
    assert_eq!(
        adapter.downloads().unwrap()[0].status,
        amp_collector::DownloadStatus::Complete
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn digital_flac_mp3_stream_seek_download_and_offline_reopen() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let fixture = Fixture::new(ItemId::new(), "CD control").await;
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
            .filter(|d| d.status == amp_collector::DownloadStatus::Complete)
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
    let fixture = Fixture::new(ItemId::new(), "CD control").await;
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
async fn connection_indicator_respects_public_streaming_policy() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    for (public, expected) in [
        (true, ConnectionState::Public),
        (false, ConnectionState::StreamingUnavailable),
    ] {
        let fixture =
            Fixture::with_media_policy(ItemId::new(), "Connection probe", false, public).await;
        let secrets = Arc::new(Secrets::default());
        let key = fixture.enroll(&secrets).await;
        let cache = tempfile::tempdir().unwrap();
        let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
        adapter
            .save_connection(
                &secrets.load(key).unwrap().unwrap(),
                fixture.pairing.endpoints[0].url.to_string(),
            )
            .unwrap();
        adapter.refresh(key, &|| false).await.unwrap();
        let probe = adapter.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || probe.connection_state(key))
                .await
                .unwrap(),
            expected
        );
        assert!(adapter.downloads().unwrap().is_empty());
        assert_eq!(
            std::fs::read_dir(cache.path().join("audio"))
                .unwrap()
                .count(),
            0
        );
        adapter.remove(key).unwrap();
        assert_eq!(
            tokio::task::spawn_blocking(move || adapter.connection_state(key))
                .await
                .unwrap(),
            ConnectionState::Disconnected
        );
    }
}

// System HTTPS roots are configured only for the child process, never the OS
// trust store or the other tests in this process.
#[test]
#[ignore = "invoked by direct_https_stream_seek_download_and_offline_with_route_indicator"]
fn direct_https_adapter_fixture() {
    use amp_audio::MediaResolver;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let root = std::path::PathBuf::from(std::env::var("TRASH_AMP_HTTPS_FIXTURE").unwrap());
    let credential: EnrolledCredential =
        serde_json::from_slice(&std::fs::read(root.join("credential.json")).unwrap()).unwrap();
    let key = ConnectionKey {
        server_id: credential.server_id,
        user_id: credential.user_id.unwrap(),
    };
    let secrets = Arc::new(Secrets::default());
    secrets.save(&credential).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets.clone()).unwrap();
    adapter
        .save_connection(&credential, credential.pairing.endpoints[0].url.to_string())
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let snapshot = runtime.block_on(adapter.refresh(key, &|| false)).unwrap();
    let expected = match std::env::var("TRASH_AMP_HTTPS_EXPECTED").unwrap().as_str() {
        "lan" => ConnectionState::Lan,
        "direct" => ConnectionState::Direct,
        "unavailable" => ConnectionState::StreamingUnavailable,
        _ => panic!("invalid fixture mode"),
    };
    assert_eq!(adapter.connection_state(key), expected);
    assert!(adapter.downloads().unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(cache.path().join("audio"))
            .unwrap()
            .count(),
        0
    );
    let track = snapshot.library.albums[0].tracks[0].clone();
    if expected == ConnectionState::StreamingUnavailable {
        // Even when public fallback is enabled, a bad advertised HTTPS route
        // must fail closed rather than sending music through the control origin.
        assert!(adapter.resolve(&track).is_err());
        adapter.schedule(std::slice::from_ref(&track)).unwrap();
        assert!(adapter.download(track).is_err());
        return;
    }
    let mut decoder =
        junk_libs_audio::AudioDecoder::open(adapter.resolve(&track).unwrap()).unwrap();
    let mut frames = 0;
    while let Some(chunk) = decoder.read_frames(1024).unwrap() {
        frames += chunk.frame_count();
    }
    assert_eq!(frames, 176400);
    decoder
        .seek_exact(junk_libs_audio::FramePosition(44100))
        .unwrap();
    assert!(decoder.read_frames(1024).unwrap().is_some());
    adapter.schedule(std::slice::from_ref(&track)).unwrap();
    adapter.download(track.clone()).unwrap();
    assert_eq!(
        adapter.downloads().unwrap()[0].status,
        amp_collector::DownloadStatus::Complete
    );
    adapter.remove(key).unwrap();
    assert_eq!(adapter.connection_state(key), ConnectionState::Disconnected);
    let mut decoder =
        junk_libs_audio::AudioDecoder::open(adapter.resolve(&track).unwrap()).unwrap();
    decoder
        .seek_exact(junk_libs_audio::FramePosition(44100))
        .unwrap();
    assert!(decoder.read_frames(1024).unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_https_stream_seek_download_and_offline_with_route_indicator() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    for local in [false, true] {
        let fixture = Fixture::with_routes(ItemId::new(), "HTTPS album", local, true, true).await;
        let secrets = Secrets::default();
        let key = fixture.enroll(&secrets).await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("credential.json"),
            serde_json::to_vec(&secrets.load(key).unwrap().unwrap()).unwrap(),
        )
        .unwrap();
        let trust = root.path().join("media-ca.pem");
        let empty = root.path().join("empty-roots");
        std::fs::create_dir(&empty).unwrap();
        std::fs::write(&trust, &fixture._remote.as_ref().unwrap().certificate).unwrap();
        for trusted in [true, false] {
            let expected = if local {
                "lan"
            } else if trusted {
                "direct"
            } else {
                "unavailable"
            };
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "direct_https_adapter_fixture",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("TRASH_AMP_HTTPS_FIXTURE", root.path())
                    .env("TRASH_AMP_HTTPS_EXPECTED", expected)
                    .env(
                        "SSL_CERT_FILE",
                        if trusted {
                            trust.clone()
                        } else {
                            empty.join("missing.pem")
                        },
                    )
                    .env("SSL_CERT_DIR", &empty)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.status.success(),
                "{expected} fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
