//! Isolated real Collector server for native UI acceptance; no production profile is opened.
#[allow(dead_code)]
#[path = "../tests/real_server.rs"]
mod fixture;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let server = fixture::Fixture::new(
        amp_collector::ids::ItemId::new(),
        "Trash Amp acceptance album",
        std::env::var_os("COLLECTOR_TEST_GAME").is_some(),
    )
    .await;
    if let Some(media) = std::env::args().nth(2) {
        let media = std::path::Path::new(&media);
        server.add_digital_purchase_files(
            &std::fs::read(media.join("01.flac"))?,
            &std::fs::read(media.join("02.mp3"))?,
            &std::fs::read(media.join("cover.png"))?,
        );
    } else {
        server.add_digital_purchase();
    }
    let output = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Supply a temporary output directory"))?;
    std::fs::create_dir_all(&output)?;
    std::fs::write(
        std::path::Path::new(&output).join("pairing.json"),
        serde_json::to_vec_pretty(&server.pairing)?,
    )?;
    std::fs::write(
        std::path::Path::new(&output).join("server.txt"),
        server.pairing.endpoints[0].url.as_str(),
    )?;
    println!("{}", server.pairing.endpoints[0].url);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
