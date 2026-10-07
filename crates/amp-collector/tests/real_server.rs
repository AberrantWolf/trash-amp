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
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.shutdown();
        self.task.abort();
    }
}
impl Fixture {
    pub async fn new(item_id: ItemId, label: &str) -> Self {
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
                            name: "disc.cue".into(),
                            bytes: ByteLength(cue.len() as u64),
                            sha256: None,
                        },
                        collector_app::UploadFile {
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
            .find(|r| r.profile == ConversionProfile::Flac)
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
