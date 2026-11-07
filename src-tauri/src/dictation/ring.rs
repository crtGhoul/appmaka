//! Lock-free SPSC ring for microphone capture.
//!
//! Why this exists: cpal's input callback runs on the audio driver's
//! thread, and anything slow there (a lock the UI thread holds, a heap
//! allocation) makes the OS drop audio. The pre-ring code locked a
//! `Mutex<Vec<f32>>` per callback and, for I16/U16 devices, allocated a
//! converted `Vec` on every callback. Here the producer only converts,
//! writes ring slots (atomic bit stores), and bumps the write index —
//! no locks, no allocation, ever. The consumer is the recorder thread,
//! which drains into a plain Vec on its existing 40 ms poll.
//!
//! One producer, one consumer. Indices are monotonic u64s (they never
//! wrap in practice: 2^64 samples at 48 kHz is ~12 million years), so
//! occupancy is just write - read and a slot is reused only after the
//! consumer has released it.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// 2^19 mono samples: ~10.9 s at 48 kHz (2 MiB). The consumer drains
/// every 40 ms, so the ring only needs to absorb scheduling hiccups —
/// seconds of headroom is far past generous.
pub const DEFAULT_CAPACITY: usize = 1 << 19;

pub struct CaptureRing {
    slots: Box<[AtomicU32]>,
    mask: usize,
    write: AtomicU64,
    read: AtomicU64,
    dropped: AtomicU64,
}

impl CaptureRing {
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity >= 2 && capacity.is_power_of_two(),
            "ring capacity must be a power of two >= 2"
        );
        Self {
            slots: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            mask: capacity - 1,
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Test-only: the production producer must drop rather than wait
    /// for room, so nothing outside tests polls occupancy.
    #[cfg(test)]
    pub fn occupancy(&self) -> u64 {
        self.write.load(Ordering::Relaxed) - self.read.load(Ordering::Relaxed)
    }

    /// Samples the producer had to drop because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Producer side (audio callback): convert-free single sample.
    /// Returns false — and counts a drop — when the ring is full; the
    /// NEW sample is dropped, never the oldest.
    pub fn push(&self, sample: f32) -> bool {
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        if w - r >= self.capacity() as u64 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.slots[(w as usize) & self.mask].store(sample.to_bits(), Ordering::Relaxed);
        // Release pairs with the consumer's Acquire on `write`, so the
        // slot store above is visible before the index moves.
        self.write.store(w + 1, Ordering::Release);
        true
    }

    /// Producer side: an F32 callback buffer, downmixed to mono with
    /// exactly the math the old Mutex<Vec> sink used (per-frame average;
    /// a single-channel buffer is copied as-is).
    pub fn push_f32(&self, data: &[f32], channels: usize) {
        if channels <= 1 {
            for &s in data {
                self.push(s);
            }
            return;
        }
        for frame in data.chunks(channels) {
            self.push(mono_frame(frame));
        }
    }

    /// Producer side: an I16 callback buffer. Conversion matches the old
    /// code exactly (divide by i16::MAX), done per channel inline so no
    /// intermediate Vec is allocated.
    pub fn push_i16(&self, data: &[i16], channels: usize) {
        let channels = channels.max(1);
        for frame in data.chunks(channels) {
            let sum: f32 = frame.iter().map(|&s| i16_to_f32(s)).sum();
            self.push(sum / frame.len() as f32);
        }
    }

    /// Producer side: a U16 callback buffer. Conversion matches the old
    /// code exactly ([0, 1] scaled to [-1, 1)).
    pub fn push_u16(&self, data: &[u16], channels: usize) {
        let channels = channels.max(1);
        for frame in data.chunks(channels) {
            let sum: f32 = frame.iter().map(|&s| u16_to_f32(s)).sum();
            self.push(sum / frame.len() as f32);
        }
    }

    /// Consumer side: append everything buffered to `out`, oldest first.
    /// Returns how many samples were moved.
    pub fn drain_into(&self, out: &mut Vec<f32>) -> usize {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        let mut i = r;
        while i != w {
            out.push(f32::from_bits(
                self.slots[(i as usize) & self.mask].load(Ordering::Relaxed),
            ));
            i += 1;
        }
        // Release pairs with the producer's Acquire on `read`, so the
        // producer never overwrites a slot before these loads finish.
        self.read.store(i, Ordering::Release);
        (w - r) as usize
    }
}

/// The old callback's conversion, pinned by tests: s / i16::MAX.
pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 / i16::MAX as f32
}

/// The old callback's conversion, pinned by tests: [0,1] → [-1,1).
pub fn u16_to_f32(s: u16) -> f32 {
    (s as f32 / u16::MAX as f32) * 2.0 - 1.0
}

/// The old sink's downmix: the frame's channels averaged.
pub fn mono_frame(frame: &[f32]) -> f32 {
    frame.iter().sum::<f32>() / frame.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the pre-ring code did: convert the whole buffer, then
    /// average each frame (extend as-is when mono).
    fn old_push_i16(data: &[i16], channels: usize) -> Vec<f32> {
        let converted: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
        old_downmix(&converted, channels)
    }

    fn old_push_u16(data: &[u16], channels: usize) -> Vec<f32> {
        let converted: Vec<f32> = data
            .iter()
            .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
            .collect();
        old_downmix(&converted, channels)
    }

    fn old_downmix(data: &[f32], channels: usize) -> Vec<f32> {
        if channels <= 1 {
            return data.to_vec();
        }
        data.chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    }

    #[test]
    fn fifo_order_survives_wraparound() {
        let ring = CaptureRing::new(8);
        for i in 0..6 {
            assert!(ring.push(i as f32));
        }
        let mut out = Vec::new();
        assert_eq!(ring.drain_into(&mut out), 4 + 2);
        assert_eq!(out, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        // Write index is now at 6 of 8: these four wrap the ring.
        for i in 6..10 {
            assert!(ring.push(i as f32));
        }
        let mut out = Vec::new();
        assert_eq!(ring.drain_into(&mut out), 4);
        assert_eq!(out, vec![6.0, 7.0, 8.0, 9.0]);
        assert_eq!(ring.dropped(), 0);
    }

    #[test]
    fn downmix_and_conversion_match_the_old_sink() {
        // I16 stereo, including full-scale and lopsided frames.
        let i16_data: Vec<i16> = vec![i16::MAX, i16::MIN, 0, 0, 1000, -1000, 32767, 1];
        let ring = CaptureRing::new(64);
        ring.push_i16(&i16_data, 2);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, old_push_i16(&i16_data, 2));

        // U16 stereo, including both rails.
        let u16_data: Vec<u16> = vec![0, u16::MAX, 32768, 32768, 12345, 54321];
        let ring = CaptureRing::new(64);
        ring.push_u16(&u16_data, 2);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, old_push_u16(&u16_data, 2));

        // Mono paths copy through conversion-identical values.
        let ring = CaptureRing::new(64);
        ring.push_i16(&i16_data, 1);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, old_push_i16(&i16_data, 1));

        let f32_data: Vec<f32> = vec![0.5, -0.5, 1.0, -1.0, 0.25, 0.75];
        let ring = CaptureRing::new(64);
        ring.push_f32(&f32_data, 2);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, old_downmix(&f32_data, 2));
        let ring = CaptureRing::new(64);
        ring.push_f32(&f32_data, 1);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, f32_data);
    }

    #[test]
    fn overflow_drops_the_new_samples_and_counts_them() {
        let ring = CaptureRing::new(4);
        for i in 0..6 {
            let ok = ring.push(i as f32);
            assert_eq!(ok, i < 4, "push {i} should succeed only while not full");
        }
        assert_eq!(ring.dropped(), 2);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        // The OLDEST samples survive; the dropped ones are the newest.
        assert_eq!(out, vec![0.0, 1.0, 2.0, 3.0]);
        // Room again after the drain.
        assert!(ring.push(99.0));
        assert_eq!(ring.dropped(), 2);
    }

    #[test]
    fn producer_consumer_threads_move_a_million_samples_losslessly() {
        let ring = std::sync::Arc::new(CaptureRing::new(4096));
        let n: u64 = 1_000_000;
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                for i in 0..n {
                    // Wait for room instead of dropping: while full the
                    // only writer is us, so occupancy can only shrink.
                    while ring.occupancy() >= ring.capacity() as u64 {
                        std::hint::spin_loop();
                    }
                    assert!(ring.push(i as f32));
                }
            })
        };
        let mut received: Vec<f32> = Vec::with_capacity(n as usize);
        while (received.len() as u64) < n {
            ring.drain_into(&mut received);
            if (received.len() as u64).is_multiple_of(65_536) {
                std::thread::yield_now();
            }
        }
        producer.join().unwrap();
        assert_eq!(ring.dropped(), 0);
        assert_eq!(received.len() as u64, n);
        for (i, &s) in received.iter().enumerate() {
            assert_eq!(s, i as f32, "sample {i} out of order");
        }
    }
}
