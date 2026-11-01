//! Audio windows of wake events, inside the audio processing thread: a ring of the
//! `VQE_OMNI_WITH_GAIN` stream (the detector's input), the hit / near-miss events of the Ricardo
//! detector and the user-reported miss. Finished buffers leave through a bounded channel; this
//! module never touches files and only allocates when an event starts or finishes.

use super::logic::{self, Kind};
use super::logic::{NEAR_AFTER_HIT_MS, NEAR_GAP_MS};
use super::{HIT_PRE_MS, MISS_MS, POST_MS, RING_MS, Recording};

/// More pending windows than this means the writer stalled: the extra event is dropped.
const MAX_PENDING: usize = 8;

/// A confirmed hit; the id is already part of the event sent to the controller, which fills the
/// outcome when the dialog ends.
#[derive(Clone, Debug, PartialEq)]
pub struct HitInfo {
    pub id: String,
    pub score: f32,
}

/// What one chunk produced.
#[derive(Debug, Default)]
pub struct Push {
    pub hit: Option<HitInfo>,
    pub near: bool,
    pub finished: Vec<Recording>,
    /// The window was not kept: too many unfinished ones are already waiting for the writer.
    pub dropped: bool,
}

struct Pending {
    rec: Recording,
    /// Chunks still to append after the ones already in `rec`.
    remaining: usize,
}

/// Ring and capture state of one processing thread.
pub(crate) struct Capture {
    frame: usize,
    ring: Vec<i16>,
    next: usize,
    filled: usize,
    seq: u64,
    /// Windowed means of the two chunks before the current one.
    prev: Option<f32>,
    prev2: Option<f32>,
    last_hit_ms: Option<i64>,
    last_near_ms: Option<i64>,
    pending: Vec<Pending>,
    cutoff: f32,
}

impl Capture {
    pub(crate) fn new(frame: usize, cutoff: f32) -> Self {
        let chunks = logic::chunks_for(u64::from(RING_MS), frame);
        Self {
            frame,
            ring: vec![0; chunks * frame],
            next: 0,
            filled: 0,
            seq: 0,
            prev: None,
            prev2: None,
            last_hit_ms: None,
            last_near_ms: None,
            pending: Vec::new(),
            cutoff,
        }
    }

    /// Recording switched off (or the detector stopped): pending windows are discarded, the ring
    /// must not keep the audio either.
    pub(crate) fn reset(&mut self) {
        self.next = 0;
        self.filled = 0;
        self.prev = None;
        self.prev2 = None;
        self.pending.clear();
    }

    fn ring_chunks(&self) -> usize {
        self.ring.len() / self.frame.max(1)
    }

    fn push_ring(&mut self, chunk: &[i16]) {
        if chunk.len() != self.frame || self.frame == 0 || self.ring.is_empty() {
            self.reset();
            return;
        }
        let at = self.next * self.frame;
        self.ring[at..at + self.frame].copy_from_slice(chunk);
        self.next = (self.next + 1) % self.ring_chunks();
        self.filled = (self.filled + 1).min(self.ring_chunks());
    }

    /// The last `n` chunks, oldest first (at most as many as stored).
    fn ring_last(&self, n: usize) -> Vec<i16> {
        let n = n.min(self.filled);
        let total = self.ring_chunks();
        let mut out = Vec::with_capacity(n * self.frame);
        for k in (1..=n).rev() {
            let at = (self.next + total - k) % total * self.frame;
            out.extend_from_slice(&self.ring[at..at + self.frame]);
        }
        out
    }

    fn id(&mut self, utc_ms: i64, kind: Kind) -> String {
        self.seq += 1;
        logic::event_id(utc_ms, kind, self.seq)
    }

    /// One detector chunk. `mean` is this chunk's windowed probability (None while the detector
    /// window fills or warms up), `hit` the score of a confirmed detection. Call only while
    /// recording is on and the Ricardo detector is fed.
    pub(crate) fn push(
        &mut self,
        chunk: &[i16],
        mean: Option<f32>,
        hit: Option<f32>,
        dry_run: bool,
        now_ms: i64,
        utc_ms: i64,
    ) -> Push {
        self.push_ring(chunk);
        let mut finished = Vec::new();
        for p in &mut self.pending {
            p.rec.samples.extend_from_slice(chunk);
            p.remaining = p.remaining.saturating_sub(1);
        }
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].remaining == 0 {
                finished.push(self.pending.remove(i).rec);
            } else {
                i += 1;
            }
        }

        if let Some(score) = hit {
            self.last_hit_ms = Some(now_ms);
            // The detector resets its own window after a hit.
            self.prev = None;
            self.prev2 = None;
            let id = self.id(utc_ms, Kind::Hit);
            let rec = Recording {
                id: id.clone(),
                kind: Kind::Hit,
                utc_ms,
                pre_ms: HIT_PRE_MS,
                post_ms: POST_MS,
                score_peak: score,
                dry_run,
                samples: self.ring_last(logic::chunks_for(u64::from(HIT_PRE_MS), self.frame)),
                label: None,
                label_source: None,
            };
            let mut dropped = false;
            if self.pending.len() < MAX_PENDING {
                self.pending.push(Pending {
                    rec,
                    remaining: logic::chunks_for(u64::from(POST_MS), self.frame),
                });
            } else {
                dropped = true;
            }
            return Push {
                hit: Some(HitInfo { id, score }),
                near: false,
                finished,
                dropped,
            };
        }

        let mut near = false;
        if let Some(cur) = mean {
            if let Some(prev) = self.prev
                && logic::near_peak(self.prev2, prev, cur, self.cutoff)
                && self
                    .last_hit_ms
                    .is_none_or(|t| now_ms.saturating_sub(t) >= NEAR_AFTER_HIT_MS)
                && self
                    .last_near_ms
                    .is_none_or(|t| now_ms.saturating_sub(t) >= NEAR_GAP_MS)
            {
                let id = self.id(utc_ms, Kind::Near);
                // The peak is the previous chunk: the window ends there, and the current chunk
                // is the first one after it.
                let mut samples =
                    self.ring_last(logic::chunks_for(u64::from(HIT_PRE_MS), self.frame) + 1);
                samples.truncate(samples.len().saturating_sub(self.frame));
                // The falling chunk is the first one after the peak.
                samples.extend_from_slice(chunk);
                let post = logic::chunks_for(u64::from(POST_MS), self.frame);
                let rec = Recording {
                    id,
                    kind: Kind::Near,
                    utc_ms,
                    pre_ms: HIT_PRE_MS,
                    post_ms: POST_MS,
                    score_peak: prev,
                    dry_run: false,
                    samples,
                    label: None,
                    label_source: None,
                };
                if self.pending.len() < MAX_PENDING {
                    self.pending.push(Pending {
                        rec,
                        remaining: post.saturating_sub(1),
                    });
                    near = true;
                    self.last_near_ms = Some(now_ms);
                }
            }
            self.prev2 = self.prev;
            self.prev = Some(cur);
        } else {
            self.prev = None;
            self.prev2 = None;
        }
        Push {
            hit: None,
            near,
            finished,
            dropped: false,
        }
    }

    /// «Wake: не услышала»: the last `MISS_MS` of the ring, no post-roll.
    pub(crate) fn miss(&mut self, utc_ms: i64) -> Option<Recording> {
        if self.filled == 0 {
            return None;
        }
        Some(Recording {
            id: self.id(utc_ms, Kind::Miss),
            kind: Kind::Miss,
            utc_ms,
            pre_ms: MISS_MS,
            post_ms: 0,
            score_peak: 0.0,
            dry_run: false,
            samples: self.ring_last(logic::chunks_for(u64::from(MISS_MS), self.frame)),
            label: Some("missed"),
            label_source: Some("user"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 512;

    fn chunk(k: usize) -> Vec<i16> {
        vec![k as i16; FRAME]
    }

    fn chunks(ms: u64) -> usize {
        logic::chunks_for(ms, FRAME)
    }

    #[test]
    fn hit_carries_three_seconds_before_and_one_after() {
        let mut c = Capture::new(FRAME, 0.9);
        // A full ring, then the confirming chunk (value 11).
        for k in 0..chunks(u64::from(RING_MS)) {
            c.push(&chunk(k), Some(0.1), None, false, 0, 0);
        }
        let p = c.push(&chunk(11), Some(0.95), Some(0.95), false, 10, 0);
        let hit = p.hit.clone().unwrap();
        assert_eq!(hit.id, "19700101T000000Z-hit-1");
        assert!(p.finished.is_empty());
        // One second of post-roll.
        let mut done = Vec::new();
        for _ in 0..chunks(u64::from(POST_MS)) {
            let p = c.push(&chunk(12), None, None, false, 20, 0);
            done.extend(p.finished);
        }
        assert_eq!(done.len(), 1);
        let rec = &done[0];
        assert_eq!(rec.kind, Kind::Hit);
        assert_eq!(
            rec.samples.len(),
            (chunks(u64::from(HIT_PRE_MS)) + chunks(u64::from(POST_MS))) * FRAME
        );
        // 3 s + 1 s of audio at 16 kHz.
        assert_eq!(rec.samples.len() / 16, 4032);
        // The window ends with the confirming chunk.
        assert_eq!(rec.samples[chunks(u64::from(HIT_PRE_MS)) * FRAME - 1], 11);
        assert_eq!(rec.score_peak, 0.95);
    }

    #[test]
    fn near_miss_ends_at_the_peak_chunk() {
        let mut c = Capture::new(FRAME, 0.9);
        for k in 0..chunks(u64::from(RING_MS)) {
            c.push(&chunk(k), Some(0.1), None, false, 0, 0);
        }
        // Rise to 0.7, then fall: the fall chunk creates the near event.
        assert!(!c.push(&chunk(300), Some(0.7), None, false, 1000, 0).near);
        let p = c.push(&chunk(301), Some(0.3), None, false, 1000 + 32, 0);
        assert!(p.near);
        assert!(p.hit.is_none());
        let mut done = Vec::new();
        for _ in 0..chunks(u64::from(POST_MS)) {
            done.extend(
                c.push(&chunk(302), Some(0.0), None, false, 2000, 0)
                    .finished,
            );
        }
        assert_eq!(done.len(), 1);
        let rec = &done[0];
        assert_eq!(rec.kind, Kind::Near);
        assert_eq!(rec.id, "19700101T000000Z-near-1");
        // 3 s ending at the peak chunk (300), then the falling one (301) and 1 s more.
        assert_eq!(
            rec.samples.len(),
            (chunks(u64::from(HIT_PRE_MS)) + chunks(u64::from(POST_MS))) * FRAME
        );
        assert_eq!(rec.samples[chunks(u64::from(HIT_PRE_MS)) * FRAME - 1], 300);
        assert_eq!(rec.samples[chunks(u64::from(HIT_PRE_MS)) * FRAME], 301);
    }

    #[test]
    fn near_misses_are_spaced_and_suppressed_around_a_hit() {
        let mut c = Capture::new(FRAME, 0.9);
        for k in 0..5 {
            c.push(&chunk(k), Some(0.1), None, false, 0, 0);
        }
        // First peak: created.
        c.push(&chunk(6), Some(0.7), None, false, 10_000, 0);
        assert!(c.push(&chunk(7), Some(0.1), None, false, 10_032, 0).near);
        // A second peak 2 s later: inside NEAR_GAP_MS.
        c.push(&chunk(8), Some(0.7), None, false, 12_000, 0);
        assert!(!c.push(&chunk(9), Some(0.1), None, false, 12_032, 0).near);
        // A hit resets the means; a peak within 2 s after it is not a near event.
        c.push(&chunk(10), Some(0.1), None, false, 20_000, 0);
        let p = c.push(&chunk(11), Some(0.97), Some(0.97), false, 20_032, 0);
        assert!(p.hit.is_some());
        c.push(&chunk(12), Some(0.6), None, false, 20_500, 0);
        c.push(&chunk(13), Some(0.8), None, false, 20_532, 0);
        assert!(!c.push(&chunk(14), Some(0.2), None, false, 20_564, 0).near);
    }

    #[test]
    fn miss_is_the_last_six_seconds_and_labeled() {
        let mut c = Capture::new(FRAME, 0.9);
        assert!(c.miss(0).is_none());
        for k in 0..chunks(u64::from(RING_MS)) {
            c.push(&chunk(k % 3000), Some(0.1), None, false, 0, 0);
        }
        let rec = c.miss(1_000_000).unwrap();
        assert_eq!(rec.kind, Kind::Miss);
        assert_eq!(rec.label, Some("missed"));
        assert_eq!(rec.label_source, Some("user"));
        assert_eq!(rec.samples.len(), chunks(u64::from(MISS_MS)) * FRAME);
        assert_eq!(
            rec.samples[rec.samples.len() - 1],
            (chunks(u64::from(RING_MS)) - 1) as i16
        );
        // Ids of a boot count every event.
        assert_eq!(rec.id, "19700101T001640Z-miss-1");
    }

    #[test]
    fn reset_drops_the_ring_and_pending_windows() {
        let mut c = Capture::new(FRAME, 0.9);
        for k in 0..10 {
            c.push(&chunk(k), Some(0.1), None, false, 0, 0);
        }
        c.push(&chunk(10), Some(0.95), Some(0.95), false, 0, 0);
        c.reset();
        assert!(c.miss(0).is_none());
    }
}
