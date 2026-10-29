//! Session lifetime follows the device stream on the audio worker.
use anyhow::{Context, Result, anyhow};
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionSetActiveOptions,
};

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_foundation::{NSNotificationCenter, NSNumber, NSObjectProtocol};
use std::sync::Arc;

pub(super) struct AudioSession {
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    active: bool,
}

impl AudioSession {
    pub fn new(shared: Arc<super::Shared>) -> Result<Self> {
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
        let mut session = Self {
            active: false,
            observers: Vec::new(),
        };
        // Activate before querying the device's actual sample rate and channels.
        session.activate()?;
        // Observe the session itself, rather than depending on a render callback
        // or UI timer to notice a disconnect. iOS may suspend both after unplugging.
        unsafe {
            use objc2_avf_audio::*;
            let center = NSNotificationCenter::defaultCenter();
            for (name, key, pause_value) in [
                (
                    AVAudioSessionRouteChangeNotification,
                    AVAudioSessionRouteChangeReasonKey,
                    AVAudioSessionRouteChangeReason::OldDeviceUnavailable.0,
                ),
                (
                    AVAudioSessionInterruptionNotification,
                    AVAudioSessionInterruptionTypeKey,
                    AVAudioSessionInterruptionType::Began.0,
                ),
            ] {
                let shared = shared.clone();
                let callback = block2::RcBlock::new(
                    move |notification: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                        let notification = notification.as_ref();
                        let value = key
                            .and_then(|key| notification.userInfo()?.objectForKey(key))
                            .and_then(|value| value.downcast::<NSNumber>().ok());
                        if value.is_some_and(|value| value.unsignedIntegerValue() == pause_value) {
                            shared.pause_for_system_event();
                        }
                    },
                );
                if let Some(name) = name {
                    session
                        .observers
                        .push(center.addObserverForName_object_queue_usingBlock(
                            Some(name),
                            None,
                            None,
                            &callback,
                        ));
                }
            }
        }
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
        // Remove callbacks before deactivation emits any new session notifications.
        let center = NSNotificationCenter::defaultCenter();
        for observer in self.observers.drain(..) {
            unsafe { center.removeObserver((&*observer).as_ref()) };
        }
        let _ = self.deactivate();
    }
}
