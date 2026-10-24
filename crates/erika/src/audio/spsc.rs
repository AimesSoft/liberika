use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{AudioPushResult, AudioRingBufferStats, apply_volume_ramp, normalize_volume};

const EMPTY_POSITION: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PcmSpscPushResult {
    pub start_position: u64,
    pub input_offset_frames: usize,
    pub accepted_frames: usize,
    pub dropped_frames: usize,
}

struct PcmSpscSlot {
    version: AtomicU64,
    position: AtomicU64,
}

impl PcmSpscSlot {
    fn empty() -> Self {
        Self {
            version: AtomicU64::new(0),
            position: AtomicU64::new(EMPTY_POSITION),
        }
    }
}

/// A bounded single-producer/single-consumer interleaved PCM ring.
///
/// Storage is allocated once. The consumer path performs only atomic loads,
/// sample copies, zero filling, and atomic counter updates. Slot
/// versions prevent a producer-side drop-oldest overwrite from exposing a torn
/// multi-channel frame to the realtime consumer.
///
/// Samples are stored at source gain. Volume is applied on the consumer side
/// as a per-callback linear ramp from the last applied gain to the current
/// target, so a control-thread volume step neither rewrites queued slots nor
/// produces an audible discontinuity (zipper noise).
pub(crate) struct PcmSpscRing {
    capacity_frames: usize,
    channels: usize,
    slots: Box<[PcmSpscSlot]>,
    samples: Box<[AtomicU32]>,
    read_position: AtomicU64,
    write_position: AtomicU64,
    written_frames: AtomicU64,
    read_frames: AtomicU64,
    dropped_frames: AtomicU64,
    underflow_frames: AtomicU64,
    target_volume: AtomicU32,
    // Only the single consumer reads and writes this between its callbacks.
    last_applied_volume: AtomicU32,
}

// The Android and HarmonyOS audio paths call all of it; tests build it on
// every platform.
#[cfg_attr(not(any(target_os = "android", target_env = "ohos")), allow(dead_code))]
impl PcmSpscRing {
    pub(crate) fn new(capacity_frames: usize, channels: usize) -> Option<Self> {
        if capacity_frames == 0 || channels == 0 {
            return None;
        }
        let sample_capacity = capacity_frames.checked_mul(channels)?;
        let slots = (0..capacity_frames)
            .map(|_| PcmSpscSlot::empty())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let samples = (0..sample_capacity)
            .map(|_| AtomicU32::new(0.0f32.to_bits()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Some(Self {
            capacity_frames,
            channels,
            slots,
            samples,
            read_position: AtomicU64::new(0),
            write_position: AtomicU64::new(0),
            written_frames: AtomicU64::new(0),
            read_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            underflow_frames: AtomicU64::new(0),
            target_volume: AtomicU32::new(1.0f32.to_bits()),
            last_applied_volume: AtomicU32::new(1.0f32.to_bits()),
        })
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    pub(crate) fn read_position(&self) -> u64 {
        self.read_position.load(Ordering::Acquire)
    }

    pub(crate) fn push_interleaved(
        &self,
        input: &[f32],
        drop_oldest_on_overflow: bool,
    ) -> PcmSpscPushResult {
        let original_frames = input.len() / self.channels;
        let mut input_offset_frames = 0usize;
        let mut candidate_frames = original_frames;
        if candidate_frames > self.capacity_frames {
            if drop_oldest_on_overflow {
                input_offset_frames = candidate_frames - self.capacity_frames;
            }
            candidate_frames = self.capacity_frames;
        }

        let write = self.write_position.load(Ordering::Relaxed);
        let mut dropped_queued_frames = 0usize;
        let accepted_frames = if drop_oldest_on_overflow {
            let minimum_read = write
                .saturating_add(candidate_frames as u64)
                .saturating_sub(self.capacity_frames as u64);
            let previous_read = self.read_position.fetch_max(minimum_read, Ordering::AcqRel);
            dropped_queued_frames = minimum_read.saturating_sub(previous_read) as usize;
            candidate_frames
        } else {
            let read = self.read_position.load(Ordering::Acquire).min(write);
            let queued = write.saturating_sub(read).min(self.capacity_frames as u64) as usize;
            candidate_frames.min(self.capacity_frames.saturating_sub(queued))
        };

        for frame_index in 0..accepted_frames {
            let position = write.saturating_add(frame_index as u64);
            let slot_index = position as usize % self.capacity_frames;
            let slot = &self.slots[slot_index];
            slot.version.fetch_add(1, Ordering::AcqRel);
            let input_frame = input_offset_frames + frame_index;
            let input_base = input_frame * self.channels;
            let output_base = slot_index * self.channels;
            for channel in 0..self.channels {
                self.samples[output_base + channel]
                    .store(input[input_base + channel].to_bits(), Ordering::Relaxed);
            }
            slot.position.store(position, Ordering::Relaxed);
            slot.version.fetch_add(1, Ordering::Release);
        }
        self.write_position.store(
            write.saturating_add(accepted_frames as u64),
            Ordering::Release,
        );

        let dropped_frames = original_frames
            .saturating_sub(accepted_frames)
            .saturating_add(dropped_queued_frames);
        self.written_frames
            .fetch_add(accepted_frames as u64, Ordering::Relaxed);
        self.dropped_frames
            .fetch_add(dropped_frames as u64, Ordering::Relaxed);

        PcmSpscPushResult {
            start_position: write,
            input_offset_frames,
            accepted_frames,
            dropped_frames,
        }
    }

    pub(crate) fn read_interleaved(&self, output: &mut [f32]) -> usize {
        let requested_frames = output.len() / self.channels;
        let requested_samples = requested_frames * self.channels;
        output[requested_samples..].fill(0.0);

        let read = self.read_position.load(Ordering::Acquire);
        let write = self.write_position.load(Ordering::Acquire);
        let available_frames = write.saturating_sub(read).min(self.capacity_frames as u64) as usize;
        let candidate_frames = requested_frames.min(available_frames);
        let mut valid_frames = 0usize;

        for frame_index in 0..candidate_frames {
            let position = read.saturating_add(frame_index as u64);
            let slot_index = position as usize % self.capacity_frames;
            let slot = &self.slots[slot_index];
            let output_base = frame_index * self.channels;
            let version_before = slot.version.load(Ordering::Acquire);
            let position_before = slot.position.load(Ordering::Acquire);
            if version_before & 1 != 0 || position_before != position {
                output[output_base..output_base + self.channels].fill(0.0);
                continue;
            }
            let sample_base = slot_index * self.channels;
            for channel in 0..self.channels {
                output[output_base + channel] =
                    f32::from_bits(self.samples[sample_base + channel].load(Ordering::Relaxed));
            }
            let version_after = slot.version.load(Ordering::Acquire);
            let read_floor = self.read_position.load(Ordering::Acquire);
            if version_before != version_after || version_after & 1 != 0 || read_floor > position {
                output[output_base..output_base + self.channels].fill(0.0);
            } else {
                valid_frames += 1;
            }
        }
        output[candidate_frames * self.channels..requested_samples].fill(0.0);
        self.read_position.fetch_max(
            read.saturating_add(candidate_frames as u64),
            Ordering::AcqRel,
        );

        // Ramp from the previous callback's gain to the current target so a
        // control-thread volume step never lands as an audible discontinuity.
        let from = f32::from_bits(self.last_applied_volume.load(Ordering::Relaxed));
        let to = f32::from_bits(self.target_volume.load(Ordering::Relaxed));
        // Only the leading `candidate_frames` carry ring data; the rest of the
        // buffer was zero-filled above and must not consume ramp progress.
        let reached = apply_volume_ramp(output, self.channels, from, to, candidate_frames);
        self.last_applied_volume
            .store(reached.to_bits(), Ordering::Relaxed);

        let underflow_frames = requested_frames.saturating_sub(valid_frames);
        self.read_frames
            .fetch_add(valid_frames as u64, Ordering::Relaxed);
        self.underflow_frames
            .fetch_add(underflow_frames as u64, Ordering::Relaxed);
        valid_frames
    }

    pub(crate) fn clear(&self) {
        let write = self.write_position.load(Ordering::Acquire);
        self.read_position.store(write, Ordering::Release);
    }

    /// Publishes a new gain target for the consumer-side ramp. Queued samples
    /// are left untouched; the next `read_interleaved` ramps toward the target
    /// without disturbing slots the realtime reader may be copying.
    pub(crate) fn set_volume(&self, volume: f32) {
        self.target_volume
            .store(normalize_volume(volume).to_bits(), Ordering::Relaxed);
    }

    /// Sets the gain target and the ramp origin together, so the next read
    /// applies `volume` as a constant instead of ramping toward it from the
    /// previous gain. Only call while the consumer is not running (for example
    /// when seeding a freshly created ring before the stream starts).
    pub(crate) fn snap_volume(&self, volume: f32) {
        let bits = normalize_volume(volume).to_bits();
        self.target_volume.store(bits, Ordering::Relaxed);
        self.last_applied_volume.store(bits, Ordering::Relaxed);
    }

    pub(crate) fn stats(&self) -> AudioRingBufferStats {
        let write = self.write_position.load(Ordering::Acquire);
        let read = self.read_position.load(Ordering::Acquire).min(write);
        let queued_frames = write.saturating_sub(read).min(self.capacity_frames as u64) as usize;
        AudioRingBufferStats {
            queued_frames,
            queued_samples: queued_frames.saturating_mul(self.channels),
            written_frames: self.written_frames.load(Ordering::Relaxed),
            read_frames: self.read_frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            underflow_frames: self.underflow_frames.load(Ordering::Relaxed),
        }
    }
}

impl From<PcmSpscPushResult> for AudioPushResult {
    fn from(value: PcmSpscPushResult) -> Self {
        Self {
            accepted_frames: value.accepted_frames,
            dropped_frames: value.dropped_frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn ring_reads_interleaved_frames_and_zero_fills_underflow() {
        let ring = PcmSpscRing::new(4, 2).unwrap();
        // Seed both ramp endpoints so this test asserts steady-state
        // amplitudes rather than the transition slope.
        ring.snap_volume(0.5);
        let pushed = ring.push_interleaved(&[0.1, 0.2, 0.3, 0.4], true);
        let mut output = [1.0; 6];

        let read = ring.read_interleaved(&mut output);

        assert_eq!(pushed.accepted_frames, 2);
        assert_eq!(read, 2);
        assert_eq!(output, [0.05, 0.1, 0.15, 0.2, 0.0, 0.0]);
        assert_eq!(ring.stats().underflow_frames, 1);
    }

    #[test]
    fn ring_drops_oldest_complete_frames_on_overflow() {
        let ring = PcmSpscRing::new(2, 2).unwrap();
        ring.push_interleaved(&[1.0, 10.0, 2.0, 20.0], true);
        let pushed = ring.push_interleaved(&[3.0, 30.0], true);
        let mut output = [0.0; 4];

        ring.read_interleaved(&mut output);

        assert_eq!(pushed.dropped_frames, 1);
        assert_eq!(output, [2.0, 20.0, 3.0, 30.0]);
        assert_eq!(ring.stats().dropped_frames, 1);
    }

    #[test]
    fn concurrent_frames_are_never_torn_between_channels() {
        const FRAMES: u32 = 20_000;
        let ring = Arc::new(PcmSpscRing::new(64, 2).unwrap());
        let producer_ring = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            for value in 1..=FRAMES {
                let sample = value as f32;
                producer_ring.push_interleaved(&[sample, -sample], true);
            }
        });

        let mut output = [0.0f32; 32];
        while !producer.is_finished() || ring.stats().queued_frames > 0 {
            ring.read_interleaved(&mut output);
            for pair in output.chunks_exact(2) {
                assert!(
                    pair == [0.0, 0.0] || pair[0] == -pair[1],
                    "torn stereo frame: {pair:?}"
                );
            }
            thread::yield_now();
        }
        producer.join().unwrap();
    }

    #[test]
    fn set_volume_leaves_queued_samples_untouched() {
        let ring = PcmSpscRing::new(2, 2).unwrap();
        ring.push_interleaved(&[1.0, -1.0, 0.5, -0.5], true);
        let versions_before: Vec<u64> = ring
            .slots
            .iter()
            .map(|slot| slot.version.load(Ordering::Acquire))
            .collect();
        let samples_before: Vec<u32> = ring
            .samples
            .iter()
            .map(|sample| sample.load(Ordering::Relaxed))
            .collect();

        ring.set_volume(0.25);

        let versions_after: Vec<u64> = ring
            .slots
            .iter()
            .map(|slot| slot.version.load(Ordering::Acquire))
            .collect();
        let samples_after: Vec<u32> = ring
            .samples
            .iter()
            .map(|sample| sample.load(Ordering::Relaxed))
            .collect();
        assert_eq!(versions_before, versions_after);
        assert_eq!(samples_before, samples_after);
        assert_eq!(
            f32::from_bits(ring.target_volume.load(Ordering::Relaxed)),
            0.25
        );
    }

    #[test]
    fn volume_step_ramps_across_the_next_read_and_then_holds() {
        let ring = PcmSpscRing::new(8, 1).unwrap();
        ring.push_interleaved(&[1.0; 8], true);

        ring.set_volume(0.5);
        let mut output = [0.0; 4];
        ring.read_interleaved(&mut output);

        // The first callback after the step ramps monotonically down to the
        // target instead of applying it as a discontinuity.
        assert_eq!(output, [0.875, 0.75, 0.625, 0.5]);

        // Subsequent callbacks apply the settled gain as a constant.
        ring.read_interleaved(&mut output);
        assert_eq!(output, [0.5, 0.5, 0.5, 0.5]);
    }
}
