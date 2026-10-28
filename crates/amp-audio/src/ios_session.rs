//! Session lifetime follows the device stream on the audio worker.
use anyhow::{Context, Result, anyhow};
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionSetActiveOptions,
};

pub(super) struct AudioSession {
    active: bool,
}

impl AudioSession {
    pub fn new() -> Result<Self> {
        // SAFETY: All session operations are serialized on the audio worker,
        // outside the UI and real-time render callbacks.
        unsafe {
            AVAudioSession::sharedInstance()
                .setCategory_error(
                    AVAudioSessionCategoryPlayback
                        .context("Playback audio category unavailable")?,
                )
                .map_err(|e| anyhow!("Cannot configure music playback: {e}"))?;
        }
        let mut session = Self { active: false };
        // Activate before querying the device's actual sample rate and channels.
        session.activate()?;
        Ok(session)
    }

    pub fn activate(&mut self) -> Result<()> {
        if !self.active {
            // SAFETY: Serialized on the audio worker.
            unsafe { AVAudioSession::sharedInstance().setActive_error(true) }
                .map_err(|e| anyhow!("Cannot activate music playback: {e}"))?;
            self.active = true;
        }
        Ok(())
    }

    pub fn deactivate(&mut self) -> Result<()> {
        if self.active {
            // SAFETY: The caller stops/drops the audio unit before deactivation.
            unsafe {
                AVAudioSession::sharedInstance().setActive_withOptions_error(
                    false,
                    AVAudioSessionSetActiveOptions::NotifyOthersOnDeactivation,
                )
            }
            .map_err(|e| anyhow!("Cannot release music playback session: {e}"))?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        let _ = self.deactivate();
    }
}
