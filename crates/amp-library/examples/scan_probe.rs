//! Read-only scanner/metadata diagnostic; its cache lives in a temporary directory.
use amp_core::Preferences;
use amp_library::{FilesystemProvider, Store, metadata};
fn main() -> anyhow::Result<()> {
    let cache = tempfile::tempdir()?;
    let store = Store::open(&cache.path().join("metadata.db"))?;
    for root in std::env::args().skip(1) {
        let start = std::time::Instant::now();
        let mut last = None;
        let mut library = FilesystemProvider {
            root: root.into(),
            preferences: Preferences::default(),
        }
        .scan_with_progress(&|| false, &mut |p| last = Some(p))?;
        println!(
            "{}: {:?}, {} albums in {:?}",
            library.root,
            last,
            library.albums.len(),
            start.elapsed()
        );
        for warning in &library.warnings {
            println!("Warning: {warning}");
        }
        if let Err(e) = metadata::enrich(&mut library, &store, &|| true, &|| false, &mut |a| {
            println!("{} — {} [{}]", a.title, a.artist, a.source_label);
        }) {
            println!("Metadata error: {e:#}");
        }
    }
    Ok(())
}
