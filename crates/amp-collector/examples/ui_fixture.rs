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
    )
    .await;
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
