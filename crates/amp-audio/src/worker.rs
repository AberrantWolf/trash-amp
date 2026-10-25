//! Decoder, queue and output lifecycle share one worker, keeping all device work off the UI.
use super::*;
use crate::output::Backend;
use std::time::{Duration, Instant};

const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) fn run<B: Backend>(
    rx: mpsc::Receiver<Command>,
    errors: mpsc::Sender<PlayerEvent>,
    shared: Arc<Shared>,
    resolver: Option<Arc<dyn MediaResolver>>,
    mut backend: B,
) {
    let mut output: Option<output::Output<B::Stream>> = None;
    let mut rate = shared.sample_rate.load(Ordering::Relaxed).max(44_100);
    let mut next_retry = Instant::now();
    let mut next_check = Instant::now();
    let mut callbacks = 0;
    let mut last_callback = Instant::now();
    let mut tracks: Vec<Track> = vec![];
    let mut index = 0;
    let mut reader: Option<Converted> = None;
    let mut position = 0;
    let mut generation = 0;
    let mut pending = None;
    let mut exhausted = true;
    while !shared.shutdown.load(Ordering::Relaxed) {
        let now = Instant::now();
        let lost = output.as_ref().is_some_and(|current| {
            let count = current.health.callbacks.load(Ordering::Relaxed);
            if count != callbacks {
                callbacks = count;
                last_callback = now;
            }
            let changed = if now >= next_check {
                next_check = now + RETRY_INTERVAL;
                backend.default_key().ok().as_ref() != Some(&current.key)
            } else {
                false
            };
            current.health.failed.load(Ordering::Acquire)
                || now.duration_since(last_callback) >= CALLBACK_TIMEOUT
                || changed
        });
        if lost {
            shared.playing.store(false, Ordering::Relaxed);
            if let Some(current) = output.take() {
                current.health.failed.store(true, Ordering::Release);
                drop(current);
            }
            if let Some(resolver) = &resolver {
                resolver.cancel();
            }
            shared.level.store(0, Ordering::Relaxed);
            shared.output_state.store(2, Ordering::Release);
            let _ = errors.send(PlayerEvent::OutputUnavailable("Audio output disconnected or changed. Playback paused; reconnecting to the default output…".into()));
            reader = None;
            pending = None;
            // Back off even when a device opens successfully but immediately fails.
            next_retry = next_retry.max(now);
        }
        if output.is_none() && now >= next_retry {
            next_retry = now + RETRY_INTERVAL;
            match backend.open(shared.clone()) {
                Ok(current) => {
                    index = shared.track.load(Ordering::Relaxed);
                    position =
                        scale_frame(shared.position.load(Ordering::Relaxed), rate, current.rate);
                    rate = current.rate;
                    shared.sample_rate.store(rate, Ordering::Release);
                    shared.position.store(position, Ordering::Relaxed);
                    generation += 1;
                    shared.generation.store(generation, Ordering::Release);
                    reader = None;
                    pending = None;
                    exhausted = index >= tracks.len();
                    callbacks = current.health.callbacks.load(Ordering::Relaxed);
                    last_callback = now;
                    next_check = now + RETRY_INTERVAL;
                    output = Some(current);
                    if shared.output_state.swap(1, Ordering::AcqRel) == 2 {
                        let _ = errors.send(PlayerEvent::OutputRecovered);
                    }
                }
                Err(error) => {
                    if shared.output_state.swap(2, Ordering::AcqRel) != 2 {
                        let _ = errors.send(PlayerEvent::OutputUnavailable(format!(
                            "Audio output unavailable: {error:#}. Reconnecting automatically…"
                        )));
                    }
                }
            }
        }
        // Device setup may have blocked while the caller closed the player.
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        while let Ok(command) = rx.try_recv() {
            // Fence the audible clock through policy evaluation and generation
            // installation. The device callback uses a single nonblocking CAS.
            let _clock = ClockGuard::lock(&shared.clock_writer);
            let mut requested_frame = 0;
            let mut reset = true;
            let mut replaced = None;
            let mut edited = None;
            match command {
                Command::Edit(edit, sequence, reply) => {
                    let audible = shared.track.load(Ordering::Relaxed);
                    let (updated, start) = edit(audible);
                    index = start.unwrap_or(audible);
                    if start.is_none() {
                        requested_frame = shared.position.load(Ordering::Relaxed);
                    } else {
                        shared.playing.store(true, Ordering::Relaxed);
                    }
                    tracks = updated;
                    replaced = Some(sequence);
                    edited = Some(reply);
                }
                Command::Replace(t, start, sequence) => {
                    replaced = Some(sequence);
                    if t.is_empty() {
                        tracks.clear();
                        index = 0;
                        shared.playing.store(false, Ordering::Relaxed);
                    } else {
                        let mut policy = QueueState::new();
                        let transition = match policy.replace(queue_tracks(&t, rate, None), start) {
                            Ok(transition) => transition,
                            Err(error) => {
                                let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                                continue;
                            }
                        };
                        tracks = t;
                        index = transition
                            .position
                            .expect("replacement has a track")
                            .track_key;
                        shared.playing.store(true, Ordering::Relaxed);
                    }
                }
                Command::Toggle => {
                    shared.playing.fetch_xor(true, Ordering::Relaxed);
                    reset = false;
                }
                Command::SetPlaying(playing) => {
                    shared.playing.store(playing, Ordering::Relaxed);
                    reset = false;
                }
                Command::Seek(seconds) => {
                    let s = if seconds.is_finite() {
                        (seconds.max(0.0) * f64::from(rate)).round() as u64
                    } else {
                        0
                    };
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    let transition = match policy.seek(Generation(generation), s) {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    let position = transition.position.expect("seek retains current track");
                    index = position.track_key;
                    requested_frame = position.frame;
                }
                Command::Next => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    index = policy
                        .next_track()
                        .ok()
                        .and_then(|transition| transition.position)
                        .map_or(tracks.len(), |position| position.track_key);
                }
                Command::Previous => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    let transition = match policy.previous_track() {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    index = transition.position.expect("previous has a track").track_key;
                }
                Command::Truncate(len) => {
                    let mut policy = match restore_audible_queue(&tracks, &shared, rate, generation)
                    {
                        Ok(policy) => policy,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    let transition = match policy.truncate(len) {
                        Ok(transition) => transition,
                        Err(error) => {
                            let _ = errors.send(PlayerEvent::PlaybackError(error.to_string()));
                            continue;
                        }
                    };
                    tracks.truncate(len);
                    if let Some(position) = transition.position {
                        index = position.track_key;
                        requested_frame = position.frame;
                    } else {
                        index = tracks.len();
                    }
                }
                Command::Append(t) => {
                    tracks.extend(t);
                    reset = false;
                    if exhausted && index < tracks.len() {
                        exhausted = false;
                    }
                }
            }
            if reset {
                generation += 1;
                shared.generation.store(generation, Ordering::Release);
                shared.track.store(index, Ordering::Relaxed);
                position = requested_frame;
                shared.position.store(position, Ordering::Relaxed);
                pending = None;
                reader = None;
                exhausted = index >= tracks.len();
                if exhausted {
                    shared.playing.store(false, Ordering::Relaxed);
                }
            }
            if let Some(sequence) = replaced {
                shared.sequence.store(sequence, Ordering::Release);
            }
            if let Some(reply) = edited {
                let _ = reply.send(tracks.clone());
            }
        }
        let Some(current) = output.as_mut() else {
            thread::sleep(Duration::from_millis(3));
            continue;
        };
        let producer = &mut current.producer;
        if !exhausted
            && shared.playing.load(Ordering::Relaxed)
            && reader.is_none()
            && pending.is_none()
        {
            if let Some(track) = tracks.get(index) {
                match Converted::new(track, position, rate, resolver.as_deref()) {
                    Ok(r) => reader = Some(r),
                    Err(e) => {
                        let _ = errors.send(PlayerEvent::PlaybackError(format!(
                            "{}: {e:#}",
                            track.title
                        )));
                        exhausted = true;
                        shared.playing.store(false, Ordering::Relaxed);
                    }
                }
            } else {
                exhausted = true;
            }
        }
        if exhausted || !shared.playing.load(Ordering::Relaxed) || producer.slots() == 0 {
            if exhausted && producer.slots() == producer.buffer().capacity() {
                shared.playing.store(false, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(3));
            continue;
        }
        if pending.is_none()
            && let Some(r) = reader.as_mut()
        {
            match r.next() {
                Ok(Some(samples)) => {
                    pending = Some(Frame {
                        samples,
                        generation,
                        track: index,
                        position,
                    });
                    position += 1;
                }
                Ok(None) => {
                    index += 1;
                    position = 0;
                    reader = None;
                }
                Err(e) => {
                    let _ = errors.send(PlayerEvent::PlaybackError(format!(
                        "Playback stopped: {e:#}"
                    )));
                    exhausted = true;
                    shared.playing.store(false, Ordering::Relaxed);
                }
            }
        }
        if let Some(frame) = pending.take()
            && let Err(rtrb::PushError::Full(f)) = producer.push(frame)
        {
            pending = Some(f);
        }
    }
}
