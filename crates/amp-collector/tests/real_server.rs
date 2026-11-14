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
        Self::with_routes(item_id, label, local, public, false, true).await
    }
    async fn with_routes(
        item_id: ItemId,
        label: &str,
        local: bool,
        public: bool,
        remote: bool,
        prepare_flac: bool,
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
            purchase_orders: vec![],
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
                // Silence in the first second distinguishes a seek from a restart.
                let sample = if frame < 44_100 {
                    0
                } else {
                    (10_000.0 * (f64::from(frame) * std::f64::consts::TAU / 100.0).sin()) as i16
                };
                let [lo, hi] = sample.to_le_bytes();
                [lo, hi, lo, hi]
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
        if prepare_flac {
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
        }
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
        while self._state.service.rebuild_next_library_entry().unwrap() {}
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
                        album_tracks: Some(
                            ["01.flac", "02.mp3"]
                                .into_iter()
                                .map(|file_name| {
                                    collector_modules::music::files::MusicImportTrack {
                                        file_name: file_name.into(),
                                        disc_position: None,
                                        display_position: None,
                                    }
                                })
                                .collect(),
                        ),
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
                    audio_preparation: None,
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
async fn refresh_retains_every_album_across_bounded_pages() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let first = ItemId::new();
    let fixture = Fixture::new(first, "Playable album").await;
    let collection = fixture
        ._state
        .service
        .item_presented(&fixture.owner, first)
        .unwrap()
        .collection_id;
    let mut expected = std::collections::BTreeSet::from([first.to_string()]);
    // Cross multiple refresh pages, including a final partial page. Albums
    // without media must also survive pagination alongside the playable album.
    for index in 0..204 {
        let id = ItemId::new();
        let item = Item {
            lifecycle: Default::default(),
            id,
            collection_id: collection,
            revision: Revision(0),
            title: format!("Album {index:02}"),
            subtitle: "Fixture artist".into(),
            identity_link: ItemIdentityLink::None,
            domain: ModuleData {
                module: ModuleId("org.junk.music".into()),
                schema_version: 1,
                payload: RawJson::from_value(serde_json::json!({"tracks": []})),
            },
            identity: IdentityState::Unidentified,
            translations: vec![],
            identifiers: vec![],
            specimens: vec![],
            wishlist: false,
            purchase_orders: vec![],
            captures: vec![],
            assets: vec![],
            metadata: Default::default(),
            deleted: false,
            updated_at: chrono::Utc::now(),
        };
        fixture
            ._state
            .service
            .save_item(&fixture.owner, id, item.into(), Revision(0), &command())
            .unwrap();
        expected.insert(id.to_string());
    }
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets).unwrap();
    let progress = Mutex::new(Vec::new());
    let snapshot = adapter
        .refresh_with_progress(key, &|| false, &|value| {
            progress.lock().unwrap().push(value)
        })
        .await
        .unwrap();
    let progress = progress.into_inner().unwrap();
    let pages = progress
        .iter()
        .filter(|p| p.total.is_some())
        .collect::<Vec<_>>();
    assert!(pages.len() > 1, "fixture must cross a page boundary");
    assert_eq!(
        pages.first().unwrap().completed,
        0,
        "count must be published before title-page progress"
    );
    assert!(
        pages
            .windows(2)
            .all(|pair| pair[0].completed < pair[1].completed)
    );
    assert!(pages.iter().all(|page| page.total == Some(expected.len())));
    assert_eq!(pages.last().unwrap().completed, expected.len());
    let actual = snapshot
        .library
        .albums
        .iter()
        .map(|album| album.id.item.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected);
    assert_eq!(snapshot.library.albums.len(), expected.len());
    assert_eq!(snapshot.album_collections.len(), expected.len());
    assert!(
        snapshot
            .library
            .albums
            .iter()
            .all(|album| album.tracks.is_empty()),
        "title refresh must not fetch album tracks"
    );
    adapter.load_album(key, first, &|| false).await.unwrap();
    assert!(
        !adapter
            .snapshot(key)
            .unwrap()
            .unwrap()
            .library
            .albums
            .iter()
            .find(|album| album.id.item == first.to_string())
            .unwrap()
            .tracks
            .is_empty()
    );
    assert_eq!(
        adapter.snapshot(key).unwrap().unwrap().library.albums.len(),
        expected.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn playback_modes_seek_original_live_and_prepared_audio() {
    use amp_audio::{MediaInput, MediaResolver, PcmReader};
    use amp_collector::PlaybackFormat;
    use junk_libs_audio::{AudioDecoder, FramePosition};
    use std::time::Duration;

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let item = ItemId::new();
    let fixture = Fixture::with_media_policy(item, "Playback format", false, true).await;
    fixture.add_digital_purchase();
    let unprepared_item = ItemId::new();
    let unprepared = Fixture::with_routes(
        unprepared_item,
        "Unprepared disc",
        false,
        true,
        false,
        false,
    )
    .await;
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let unprepared_key = unprepared.enroll(&secrets).await;
    let cache = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(cache.path(), secrets).unwrap();
    let snapshot = refresh_open(&adapter, key).await;
    refresh_open(&adapter, unprepared_key).await;
    let unprepared_track = adapter
        .load_album(unprepared_key, unprepared_item, &|| false)
        .await
        .unwrap()
        .tracks
        .remove(0);
    let album = snapshot
        .library
        .albums
        .iter()
        .find(|album| album.title == "Purchased album")
        .unwrap();
    let digital_tracks = album.tracks.clone();
    let track = adapter
        .load_album(key, item, &|| false)
        .await
        .unwrap()
        .tracks
        .remove(0);

    tokio::task::spawn_blocking(move || {
        let position = Duration::from_secs(2);
        for (format, media_type) in [
            (PlaybackFormat::Original, "audio/wav"),
            (PlaybackFormat::HighBandwidth, "audio/flac"),
        ] {
            adapter.set_playback_format(format).unwrap();
            let MediaInput::Encoded(source) = adapter.open_at(&track, position).unwrap() else {
                panic!("Expected seekable {format:?} audio");
            };
            assert_eq!(source.hint().media_type.as_deref(), Some(media_type));
            let mut reader = AudioDecoder::open(source).unwrap();
            assert_eq!(reader.format().sample_rate(), 44_100);
            reader.seek_exact(FramePosition(88_200)).unwrap();
            assert_eq!(assert_audible_tail(&mut reader), 88_200);
        }

        assert_eq!(
            unprepared_track.availability,
            amp_core::Availability::Available
        );
        let MediaInput::Streamed(mut reader) =
            adapter.open_at(&unprepared_track, position).unwrap()
        else {
            panic!("Expected live FLAC for the unprepared disc");
        };
        assert_eq!(reader.format().sample_rate(), 44_100);
        assert_eq!(assert_audible_tail(reader.as_mut()), 88_200);

        // High Bandwidth must accept the original MP3 advertised by Collector.
        let MediaInput::Encoded(source) = adapter.open(&digital_tracks[1]).unwrap() else {
            panic!("Expected the original MP3");
        };
        assert_eq!(source.hint().media_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(
            AudioDecoder::open(source).unwrap().format().sample_rate(),
            44_100
        );

        let format = PlaybackFormat::LowBandwidth { bitrate_kbps: 113 };
        adapter.set_playback_format(format).unwrap();
        let MediaInput::Streamed(mut reader) = adapter.open_at(&track, position).unwrap() else {
            panic!("Expected live Opus");
        };
        assert_eq!(reader.format().sample_rate(), 48_000);
        assert_eq!(assert_audible_tail(reader.as_mut()), 96_000);

        adapter.schedule(std::slice::from_ref(&track)).unwrap();
        adapter.download(track.clone()).unwrap();
        let MediaInput::Encoded(source) = adapter.open(&track).unwrap() else {
            panic!("Expected the retained FLAC");
        };
        assert_eq!(source.hint().media_type.as_deref(), Some("audio/flac"));
    })
    .await
    .unwrap();

    fn assert_audible_tail(reader: &mut dyn PcmReader) -> u64 {
        let first = reader.read_frames(1024).unwrap().unwrap();
        assert!(first.samples.iter().any(|sample| sample.abs() > 0.1));
        let mut decoded = first.frame_count();
        while let Some(chunk) = reader.read_frames(1024).unwrap() {
            decoded += chunk.frame_count();
        }
        decoded
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
    for key in [one, two] {
        let credential = secrets.load(key).unwrap().unwrap();
        adapter
            .save_connection(&credential, credential.pairing.endpoints[0].url.to_string())
            .unwrap();
    }
    let a = refresh_open(&adapter, one).await;
    let b = refresh_open(&adapter, two).await;
    adapter.select(Some(two)).unwrap();

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
    let snapshot = refresh_open(&adapter, one).await;
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
        use amp_audio::{MediaInput, MediaResolver};
        a.set_playback_format(amp_collector::PlaybackFormat::HighBandwidth)
            .unwrap();
        let MediaInput::Encoded(source) = a.open(&t).unwrap() else {
            panic!("A separate media connection should use saved audio");
        };
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
    let snapshot = refresh_open(&adapter, key).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn artwork_publishes_and_persists_successes_despite_other_cover_failure_or_cancellation() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let item = ItemId::new();
    let fixture = Fixture::new(item, "Cover control").await;
    fixture.add_digital_purchase();
    let service = &fixture._state.service;
    let original = service.item_presented(&fixture.owner, item).unwrap();
    let cover = service
        .create_asset(
            &fixture.owner,
            collector_app::CreateAssetCommand {
                collection_id: original.collection_id,
                subject: AssetSubject::Item { item_id: item },
                kind: AssetKind::Cover,
                visibility: Visibility::Shared,
                alt_text: "Control cover".into(),
            },
            include_bytes!("fixtures/digital-audio/cover.png"),
            &command(),
        )
        .unwrap();
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let root = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(root.path(), secrets.clone()).unwrap();
    let snapshot = adapter.refresh(key, &|| false).await.unwrap();
    assert_eq!(snapshot.artwork_descriptors.len(), 2);
    service
        .tombstone_asset(&fixture.owner, cover.id, cover.revision, &command())
        .unwrap();
    let publications = Mutex::new(Vec::new());
    let error = adapter
        .refresh_artwork_incremental(key, &|| false, &|artwork| {
            for path in artwork.values() {
                assert!(
                    std::fs::read(path)
                        .unwrap()
                        .starts_with(b"\x89PNG\r\n\x1a\n")
                );
            }
            publications.lock().unwrap().push(artwork);
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("1 covers could not be loaded"));
    assert!(
        error.to_string().contains("not found"),
        "keep the actual request failure visible: {error}"
    );
    assert_eq!(publications.lock().unwrap().len(), 1);
    let saved = adapter.snapshot(key).unwrap().unwrap();
    assert_eq!(saved.artwork.len(), 1);
    assert!(!saved.artwork.contains_key(&item.to_string()));

    // A fresh cache cancels immediately after its first successful publication.
    // Both covers are valid again through a fresh, current title descriptor.
    service
        .create_asset(
            &fixture.owner,
            collector_app::CreateAssetCommand {
                collection_id: original.collection_id,
                subject: AssetSubject::Item { item_id: item },
                kind: AssetKind::Cover,
                visibility: Visibility::Shared,
                alt_text: "Replacement cover".into(),
            },
            include_bytes!("fixtures/digital-audio/cover.png"),
            &command(),
        )
        .unwrap();
    while service.rebuild_next_library_entry().unwrap() {}
    let other_root = tempfile::tempdir().unwrap();
    let other = Adapter::with_credentials(other_root.path(), secrets).unwrap();
    other.refresh(key, &|| false).await.unwrap();
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let error = other
        .refresh_artwork_incremental(
            key,
            &|| cancelled.load(std::sync::atomic::Ordering::Acquire),
            &|artwork| {
                assert_eq!(artwork.len(), 1);
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Cancelled"));
    assert_eq!(other.snapshot(key).unwrap().unwrap().artwork.len(), 1);
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
    let snapshot = refresh_open(&adapter, key).await;
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
        refresh_open(&adapter, key).await;
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
    let snapshot = runtime.block_on(refresh_open(&adapter, key));
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
        // Separate audio is required, and the advertised HTTPS route is untrusted.
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
        // Collector advertises separate media when normal audio is disabled.
        let fixture =
            Fixture::with_routes(ItemId::new(), "HTTPS album", local, false, true, true).await;
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

// Media journeys explicitly load the album/list/artwork layers they exercise.
#[cfg(test)]
async fn refresh_open(
    adapter: &amp_collector::Adapter,
    key: ConnectionKey,
) -> amp_collector::Snapshot {
    let snapshot = adapter.refresh(key, &|| false).await.unwrap();
    for album in &snapshot.library.albums {
        adapter
            .load_album(key, album.id.item.parse().unwrap(), &|| false)
            .await
            .unwrap();
    }
    for list in &snapshot.lists {
        adapter
            .load_list(key, list.list.id, &|| false)
            .await
            .unwrap();
    }
    adapter.refresh_artwork(key, &|| false).await.unwrap();
    adapter.snapshot(key).unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_album_loads_before_the_title_refresh_commits_its_manifest() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let item = ItemId::new();
    let fixture = Fixture::new(item, "Selected album").await;
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    let root = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(root.path(), secrets).unwrap();
    let loaded = Mutex::new(false);
    let snapshot = adapter
        .refresh_incremental(key, &|| false, &|_| {}, &|page| {
            if page
                .library
                .albums
                .iter()
                .any(|a| a.id.item == item.to_string())
            {
                assert!(
                    adapter.snapshot(key).unwrap().is_none(),
                    "the final manifest must still be uncommitted"
                );
                let a = adapter.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let _ = tx.send(runtime.block_on(a.load_album(key, item, &|| false)));
                });
                let album = rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("album load stalled behind title refresh")
                    .unwrap();
                assert!(!album.tracks.is_empty());
                *loaded.lock().unwrap() = true;
            }
        })
        .await
        .unwrap();
    assert!(*loaded.lock().unwrap());
    assert!(!snapshot.library.albums[0].tracks.is_empty());
    let search = adapter.search_tracks(key, "Song", &|| false).await.unwrap();
    assert!(
        !search.albums.is_empty(),
        "online song search must work independently of album opening"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_requeries_counts_and_pages_until_pending_details_settle_without_loading_tracks() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let fixture = Fixture::new(ItemId::new(), "Existing album").await;
    let secrets = Arc::new(Secrets::default());
    let key = fixture.enroll(&secrets).await;
    // Publication changes the catalog after the first background backfill.
    fixture.add_digital_purchase();
    let root = tempfile::tempdir().unwrap();
    let adapter = Adapter::with_credentials(root.path(), secrets).unwrap();
    let observed_pending = std::sync::atomic::AtomicBool::new(false);
    let result = adapter
        .refresh_catalog(key, &|| false, &|_| {}, &|snapshot| {
            if snapshot.summaries_pending {
                observed_pending.store(true, std::sync::atomic::Ordering::Release);
                while fixture._state.service.rebuild_next_library_entry().unwrap() {}
            }
        })
        .await
        .unwrap();
    assert!(observed_pending.load(std::sync::atomic::Ordering::Acquire));
    assert!(!result.summaries_pending);
    assert_eq!(result.library.albums.len(), 2);
    assert!(
        result
            .library
            .albums
            .iter()
            .all(|album| album.tracks.is_empty()),
        "retries must remain title-only reads"
    );
}
