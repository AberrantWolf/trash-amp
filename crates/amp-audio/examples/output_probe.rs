//! Exercises device output with silence; does not require or modify a user's music.
use amp_audio::Player;
use amp_core::{ItemId, MediaAccess, Track};
use std::{
    thread,
    time::{Duration, Instant},
};
fn main() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("silence.wav");
    let mut wav = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;
    for _ in 0..44100 * 2 * 3 {
        wav.write_sample(0i16)?;
    }
    wav.finalize()?;
    let player = Player::new()?;
    player.play(
        vec![Track {
            blocked_reason: None,
            availability: Default::default(),
            source: None,
            id: ItemId {
                library: "probe".into(),
                provider: "file".into(),
                item: "silence".into(),
            },
            title: "Output probe".into(),
            artist: String::new(),
            number: 1,
            disc: 1,
            duration_seconds: 3.0,
            access: MediaAccess::File {
                locator: path.to_string_lossy().into_owned(),
            },
        }],
        0,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(error) = player.error() {
            anyhow::bail!(error)
        }
        let state = player.snapshot();
        if state.seconds > 1.0 {
            println!(
                "Output clock advanced to {:.3}s; {} callback underruns (includes startup).",
                state.seconds, state.underruns
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("Output clock did not advance")
}
