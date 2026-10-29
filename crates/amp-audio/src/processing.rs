//! Output-rate stereo effects. State belongs exclusively to the render callback.
use amp_core::{AudioProcessingSettings, CrossfeedSettings};
use bs2b::{Bs2b, Level};

// A single atomic publishes a coherent configuration, including rapid UI edits.
// Cutoff uses 11 bits, attenuation 8 bits, and enabled 1 bit.
pub(super) fn pack(settings: AudioProcessingSettings) -> u32 {
    let c = settings.normalized().crossfeed;
    c.cutoff_hz | (c.feed_db_tenths << 11) | (u32::from(c.enabled) << 19)
}
fn unpack(value: u32) -> CrossfeedSettings {
    CrossfeedSettings {
        enabled: value & (1 << 19) != 0,
        cutoff_hz: value & 0x7ff,
        feed_db_tenths: (value >> 11) & 0xff,
    }
    .normalized()
}

/// Output-rate chain. Add stages here without changing decoding or buffering.
/// Its state and every stage's storage are fixed-size and callback-owned.
pub(super) struct ProcessingChain {
    crossfeed: Crossfeed,
    generation: Option<u64>,
}
impl ProcessingChain {
    pub fn new(rate: u32, channels: usize) -> Self {
        Self {
            crossfeed: Crossfeed::new(rate, channels),
            generation: None,
        }
    }

    pub fn begin_callback(&mut self, generation: u64, settings: u32) {
        self.crossfeed.requested = unpack(settings);
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }

    /// Discontinuities reset all stages; pauses leave their history frozen.
    pub fn reset(&mut self) {
        self.crossfeed.reset();
    }

    pub fn process_frame(&mut self, input: [f32; 2]) -> [f32; 2] {
        self.crossfeed.process_frame(input)
    }
}

struct Crossfeed {
    processor: Option<Bs2b>,
    requested: CrossfeedSettings,
    wet_frames: u32,
    ramp_frames: u32,
}
impl Crossfeed {
    fn new(rate: u32, channels: usize) -> Self {
        Self {
            processor: if channels >= 2 {
                Bs2b::new(rate, Level::DEFAULT).ok()
            } else {
                None
            },
            requested: CrossfeedSettings::default(),
            wet_frames: 0,
            ramp_frames: rate.div_ceil(100).max(1),
        }
    }

    fn reset(&mut self) {
        if let Some(processor) = &mut self.processor {
            processor.clear();
        }
        self.wet_frames = 0;
    }

    pub fn process_frame(&mut self, input: [f32; 2]) -> [f32; 2] {
        if !self.requested.enabled && self.wet_frames == 0 {
            return input;
        }
        let Some(crossfeed) = &mut self.processor else {
            return input;
        };
        let changed = crossfeed.level().cut_frequency_hz() != self.requested.cutoff_hz
            || crossfeed.level().feed_db_tenths() != self.requested.feed_db_tenths;
        if changed && self.wet_frames == 0 {
            // Validated integer settings; constructing coefficients is bounded
            // and allocation-free. Reconfigure only after fading to dry.
            let level = Level::new(self.requested.cutoff_hz, self.requested.feed_db_tenths)
                .expect("normalized crossfeed level");
            crossfeed.set_level(level);
            crossfeed.clear();
        }
        let fading_out = !self.requested.enabled || (changed && self.wet_frames > 0);
        self.wet_frames = if fading_out {
            self.wet_frames.saturating_sub(1)
        } else {
            (self.wet_frames + 1).min(self.ramp_frames)
        };
        if self.wet_frames == 0 {
            crossfeed.clear();
            return input;
        }
        let (left, right) = crossfeed.process_frame(input[0], input[1]);
        if self.wet_frames == self.ramp_frames {
            return [left, right];
        }
        let wet = self.wet_frames as f32 / self.ramp_frames as f32;
        [
            input[0] + wet * (left - input[0]),
            input[1] + wet * (right - input[1]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(enabled: bool, cutoff_hz: u32, feed_db_tenths: u32) -> u32 {
        pack(AudioProcessingSettings {
            crossfeed: CrossfeedSettings {
                enabled,
                cutoff_hz,
                feed_db_tenths,
            },
        })
    }
    fn signal(i: u32) -> [f32; 2] {
        [
            (i as f32 * 0.071).sin() * 0.8,
            (i as f32 * 0.093).cos() * 0.5,
        ]
    }

    #[test]
    fn atomic_configuration_round_trips_and_normalizes() {
        for enabled in [false, true] {
            for (cutoff, feed) in [(0, 0), (300, 10), (2000, 150), (u32::MAX, u32::MAX)] {
                let original = CrossfeedSettings {
                    enabled,
                    cutoff_hz: cutoff,
                    feed_db_tenths: feed,
                };
                assert_eq!(
                    unpack(pack(AudioProcessingSettings {
                        crossfeed: original
                    })),
                    original.normalized()
                );
                Level::new(
                    original.normalized().cutoff_hz,
                    original.normalized().feed_db_tenths,
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn disabled_and_unsupported_outputs_are_exact_bypasses() {
        for (rate, channels, enabled) in [
            (44100, 2, false),
            (48000, 1, true),
            (1999, 2, true),
            (384001, 2, true),
        ] {
            let mut chain = ProcessingChain::new(rate, channels);
            chain.begin_callback(1, settings(enabled, 700, 45));
            // Bypass must not even clamp or change the sign of zero.
            for input in [[-0.0, 0.0], [1.2, -1.1], [0.23, -0.79]] {
                let actual = chain.process_frame(input);
                assert_eq!(actual.map(f32::to_bits), input.map(f32::to_bits));
            }
        }
    }

    #[test]
    fn presets_match_bs2b_and_toggles_ramp_in_exactly_ten_milliseconds() {
        for rate in [2000, 44100, 48000, 96000, 192000, 384000] {
            for level in [
                Level::DEFAULT,
                Level::CMOY,
                Level::JMEIER,
                Level::new(1700, 113).unwrap(),
            ] {
                let mut chain = ProcessingChain::new(rate, 2);
                let enabled = settings(true, level.cut_frequency_hz(), level.feed_db_tenths());
                chain.begin_callback(1, enabled);
                let mut reference = Bs2b::new(rate, level).unwrap();
                let ramp = rate.div_ceil(100);
                for i in 0..ramp + 500 {
                    let input = signal(i);
                    let (l, r) = reference.process_frame(input[0], input[1]);
                    let wet = ((i + 1) as f32 / ramp as f32).min(1.0);
                    let expected = if wet == 1.0 {
                        [l, r]
                    } else {
                        [
                            input[0] + wet * (l - input[0]),
                            input[1] + wet * (r - input[1]),
                        ]
                    };
                    assert_eq!(chain.process_frame(input), expected);
                }
                chain.begin_callback(
                    1,
                    settings(false, level.cut_frequency_hz(), level.feed_db_tenths()),
                );
                for i in 0..ramp {
                    chain.process_frame(signal(i));
                    assert_eq!(chain.crossfeed.wet_frames, ramp - i - 1);
                }
                assert_eq!(
                    chain.process_frame([-0.0, 1.2]).map(f32::to_bits),
                    [-0.0_f32, 1.2].map(f32::to_bits)
                );
            }
        }
    }

    #[test]
    fn crossfeed_reaches_other_channel_and_is_independent_of_callback_boundaries() {
        let enabled = settings(true, 700, 45);
        let mut single = ProcessingChain::new(48000, 2);
        single.begin_callback(1, enabled);
        let expected: Vec<_> = (0..3000).map(|i| single.process_frame(signal(i))).collect();
        for size in [1, 17, 128, 512, 1024] {
            let mut chunked = ProcessingChain::new(48000, 2);
            let mut actual = Vec::new();
            for start in (0..3000).step_by(size) {
                chunked.begin_callback(1, enabled);
                for i in start..(start + size).min(3000) {
                    actual.push(chunked.process_frame(signal(i as u32)));
                }
            }
            assert_eq!(actual, expected);
        }
        let mut chain = ProcessingChain::new(48000, 2);
        chain.begin_callback(1, enabled);
        for _ in 0..480 {
            chain.process_frame([0.0, 0.0]);
        }
        assert!(chain.process_frame([1.0, 0.0])[1] > 0.0);
    }

    #[test]
    fn parameter_edits_fade_through_dry_and_coalesce_to_latest() {
        let mut chain = ProcessingChain::new(48000, 2);
        chain.begin_callback(1, settings(true, 700, 45));
        for i in 0..1000 {
            chain.process_frame(signal(i));
        }
        chain.begin_callback(1, settings(true, 700, 60));
        for i in 0..200 {
            chain.process_frame(signal(i));
        }
        chain.begin_callback(1, settings(true, 650, 95));
        for i in 0..280 {
            chain.process_frame(signal(i));
        }
        assert_eq!(chain.crossfeed.wet_frames, 0);
        assert_eq!(
            chain.crossfeed.processor.as_ref().unwrap().level(),
            Level::DEFAULT
        );
        chain.process_frame([0.0, 0.0]);
        assert_eq!(
            chain.crossfeed.processor.as_ref().unwrap().level(),
            Level::JMEIER
        );
        assert_eq!(chain.crossfeed.wet_frames, 1);
        for i in 0..479 {
            chain.process_frame(signal(i));
        }
        assert_eq!(chain.crossfeed.wet_frames, 480);
    }

    #[test]
    fn discontinuities_clear_history_while_same_generation_preserves_it() {
        let enabled = settings(true, 700, 45);
        let mut chain = ProcessingChain::new(48000, 2);
        chain.begin_callback(1, enabled);
        for i in 0..1000 {
            chain.process_frame(signal(i));
        }
        let wet = chain.crossfeed.wet_frames;
        chain.begin_callback(1, enabled);
        assert_eq!(chain.crossfeed.wet_frames, wet);
        assert!(!chain.crossfeed.processor.as_ref().unwrap().is_clear());
        chain.begin_callback(2, enabled);
        assert_eq!(chain.process_frame([0.0, 0.0]), [0.0, 0.0]);
        for i in 0..1000 {
            chain.process_frame(signal(i));
        }
        chain.reset();
        assert_eq!(chain.process_frame([0.0, 0.0]), [0.0, 0.0]);
    }
}
