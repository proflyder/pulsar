//! Experimental local end-of-utterance detector for the voice pipeline.
//!
//! The only generic signal the front-end has is the energy of each 32 ms VQE chunk. Yandex VQE
//! exposes no speech probability, and the phrase spotters are word models, not a VAD. The
//! detector is therefore a relative energy VAD: all thresholds are derived from the wake word of
//! the same turn (its level `S` and the quiet part `N` of the same 1.5 s window), so a quiet
//! speaker in a quiet room and a loud one in a noisy room get different absolute thresholds.
//! The coefficients below are deliberately *not* claimed as measured calibration; they are
//! starting values for live tuning (see `docs/STATE.md`).
//!
//! It is only a hint for Home Assistant: the device sends one graceful `audio.end` when the
//! phrase looks finished. Home Assistant's own VAD stays authoritative and may end the run
//! first. A turn without a usable wake level runs without a detector.

/// One VQE chunk at 16 kHz.
pub const CHUNK_MS: u32 = 32;
/// Consecutive chunks above the onset threshold before the detector believes speech started.
/// 5 * 32 ms = 160 ms: long enough to reject clicks, short enough for a short "отмена".
pub const ONSET_CHUNKS: u32 = 5;
/// Chunks below the release threshold that end the phrase. 31 * 32 ms = 992 ms: ordinary
/// pauses inside a sentence are shorter, while the wait is still much shorter than the
/// 30 s listening watchdog.
pub const TRAILING_SILENCE_CHUNKS: u32 = 31;
/// No detector below this wake-word SNR: the signal does not separate speech from the floor.
pub const MIN_PRIOR_RANGE_DB: f32 = 3.0;
/// Onset sits half-way (in dB) between the measured floor and the wake word level.
pub const ONSET_FRACTION: f32 = 0.5;
/// Release sits a quarter of the way above the floor: hysteresis against level jitter.
pub const RELEASE_FRACTION: f32 = 0.25;

/// Levels of the wake word (`speech_db`) and of the quiet part of its window (`noise_db`), both
/// mean-square dBFS measured on the same VQE channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EndpointPrior {
    pub noise_db: f32,
    pub speech_db: f32,
}

impl EndpointPrior {
    /// Onset / release thresholds, or None when the wake word does not stand out from the floor.
    pub fn thresholds(&self) -> Option<(f32, f32)> {
        if !self.noise_db.is_finite() || !self.speech_db.is_finite() {
            return None;
        }
        let range = self.speech_db - self.noise_db;
        if range < MIN_PRIOR_RANGE_DB {
            return None;
        }
        Some((
            self.noise_db + ONSET_FRACTION * range,
            self.noise_db + RELEASE_FRACTION * range,
        ))
    }
}

/// Result of feeding one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Keep listening.
    Continue,
    /// The phrase ended: send one graceful `audio.end`.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Waiting,
    Speech,
    Ended,
}

/// Per-dialog detector. Feed pre-roll chunks with `post_wake = false`, then live chunks with
/// `post_wake = true`.
///
/// An end is only reported after sustained speech was seen *after* the wake word was handled.
/// Pre-roll alone cannot prove a command: the 256 ms Ricardo pre-roll ends at the confirmation
/// and may contain only the tail of "Рикардо". A detector that ended on pre-roll speech plus live
/// silence would close the microphone on every wake word, so pre-roll evidence arms the state
/// machine but never completes a phrase. Live evidence must also last [`ONSET_CHUNKS`] above the
/// *onset* threshold: short bursts under 160 ms do not qualify. This is a duration guard, not a
/// guarantee — a loud earcon or wake tail that the AEC does not remove and lasts as long looks
/// like speech (see `docs/STATE.md`).
pub struct EndpointDetector {
    prior: EndpointPrior,
    on_db: f32,
    off_db: f32,
    state: State,
    above: u32,
    /// Consecutive live chunks at the onset threshold (post-wake evidence).
    live_run: u32,
    trailing: u32,
    saw_post_wake_speech: bool,
}

impl EndpointDetector {
    pub fn new(prior: EndpointPrior) -> Option<Self> {
        let (on_db, off_db) = prior.thresholds()?;
        Some(Self {
            prior,
            on_db,
            off_db,
            state: State::Waiting,
            above: 0,
            live_run: 0,
            trailing: 0,
            saw_post_wake_speech: false,
        })
    }

    pub fn prior(&self) -> EndpointPrior {
        self.prior
    }

    pub fn thresholds(&self) -> (f32, f32) {
        (self.on_db, self.off_db)
    }

    /// Re-arms a finished phrase after resumed speech: the same dialog may still end later.
    pub fn reopen(&mut self) {
        if self.state == State::Ended {
            self.state = State::Speech;
            self.above = 0;
            self.trailing = 0;
            self.live_run = ONSET_CHUNKS;
        }
    }

    /// Live evidence must itself last: pre-roll may fill [`Self::above`] but never counts here,
    /// and five live chunks starting from `Waiting` qualify exactly as five later ones do.
    fn live_evidence(&mut self, level_db: f32, post_wake: bool) {
        if !post_wake {
            return;
        }
        if level_db >= self.on_db {
            self.live_run += 1;
            if self.live_run >= ONSET_CHUNKS {
                self.saw_post_wake_speech = true;
            }
        } else {
            self.live_run = 0;
        }
    }

    pub fn push(&mut self, level_db: f32, post_wake: bool) -> Step {
        if self.state != State::Ended {
            self.live_evidence(level_db, post_wake);
        }
        match self.state {
            State::Waiting => {
                if level_db >= self.on_db {
                    self.above += 1;
                } else {
                    self.above = 0;
                }
                if self.above >= ONSET_CHUNKS {
                    self.state = State::Speech;
                    self.above = 0;
                    self.trailing = 0;
                }
                Step::Continue
            }
            State::Speech => {
                if level_db <= self.off_db {
                    self.trailing += 1;
                } else {
                    self.trailing = 0;
                }
                if self.trailing < TRAILING_SILENCE_CHUNKS {
                    return Step::Continue;
                }
                self.trailing = 0;
                if self.saw_post_wake_speech {
                    self.state = State::Ended;
                    Step::End
                } else {
                    // Only pre-roll (wake tail) was seen: say nothing, keep waiting. A later
                    // command is detected from scratch.
                    self.state = State::Waiting;
                    self.above = 0;
                    self.live_run = 0;
                    Step::Continue
                }
            }
            State::Ended => Step::Continue,
        }
    }
}

/// One dialog's endpointing lifecycle: the detector plus the "one graceful end" bookkeeping,
/// separated from the controller so the lifecycle is testable without actors.
pub struct EndpointRun {
    detector: EndpointDetector,
    /// An end is due but was not sent yet (the wake earcon is playing).
    pending: bool,
    sent: bool,
}

impl EndpointRun {
    /// Starts a run. `preroll` is pre-wake evidence, `live` is post-wake audio held during wake
    /// arbitration.
    pub fn new(prior: EndpointPrior, preroll: &[i16], live: &[i16]) -> Option<Self> {
        let detector = EndpointDetector::new(prior)?;
        let mut run = Self {
            detector,
            pending: false,
            sent: false,
        };
        for chunk in preroll.chunks(512) {
            run.push(pcm_level_db(chunk), false);
        }
        // The arbitration backlog can exceed the trailing-silence window on a delayed
        // scheduler, so feed it through the same path: an end found there must not be lost.
        for chunk in live.chunks(512) {
            run.push(pcm_level_db(chunk), true);
        }
        Some(run)
    }

    /// Feeds one chunk. Speech that resumes after a deferred end cancels that end: the phrase
    /// was not finished after all.
    pub fn push(&mut self, level_db: f32, post_wake: bool) {
        match self.detector.push(level_db, post_wake) {
            Step::End => {
                if !self.sent {
                    self.pending = true;
                }
            }
            Step::Continue => {
                if self.pending && level_db >= self.detector.thresholds().1 {
                    self.pending = false;
                    // The phrase continued; a later silence may end it again.
                    self.detector.reopen();
                }
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending
    }

    /// Takes the due end, at most once per run. False when nothing is due (or already sent).
    pub fn take_end(&mut self) -> bool {
        if !std::mem::take(&mut self.pending) || self.sent {
            return false;
        }
        self.sent = true;
        true
    }

    /// The dialog ended elsewhere (HA finished first, cancel, repeated wake): nothing may be
    /// sent for it afterwards.
    pub fn cancel(&mut self) {
        self.pending = false;
        self.sent = true;
    }

    pub fn prior(&self) -> EndpointPrior {
        self.detector.prior()
    }

    pub fn thresholds(&self) -> (f32, f32) {
        self.detector.thresholds()
    }
}

/// Mean-square dBFS of s16 PCM, clamped like the front-end's levels.
pub fn pcm_level_db(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return -100.0;
    }
    let mut sq = 0.0f64;
    for &s in pcm {
        let v = f64::from(s) * (1.0 / 32768.0);
        sq = v.mul_add(v, sq);
    }
    mean_square_db((sq / pcm.len() as f64) as f32)
}

/// Mean-square (linear power) -> dBFS, clamped to -100..0.
pub fn mean_square_db(ms: f32) -> f32 {
    (10.0 * (ms.max(0.0) + 1e-10).log10()).clamp(-100.0, 0.0)
}

/// Quiet-part estimate: dB level at quantile `q` (0..=1) of a window of mean squares.
pub fn percentile_db(window: &[f32], q: f32) -> f32 {
    if window.is_empty() {
        return -100.0;
    }
    let mut s = window.to_vec();
    s.sort_unstable_by(f32::total_cmp);
    let i = ((s.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    mean_square_db(s[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prior() -> EndpointPrior {
        // A 30 dB wake-word range: onset -40 dB, release -47.5 dB.
        EndpointPrior {
            noise_db: -55.0,
            speech_db: -25.0,
        }
    }

    fn push(d: &mut EndpointDetector, level: f32, n: usize, post_wake: bool) -> Vec<Step> {
        (0..n).map(|_| d.push(level, post_wake)).collect()
    }

    #[test]
    fn thresholds_are_relative_to_the_wake_word() {
        let (on, off) = prior().thresholds().unwrap();
        assert_eq!(on, -40.0);
        assert_eq!(off, -47.5);
        assert!(
            EndpointDetector::new(EndpointPrior {
                noise_db: -50.0,
                speech_db: -48.0,
            })
            .is_none()
        );
    }

    #[test]
    fn waiting_silence_never_ends() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        assert!(
            push(&mut d, -60.0, 1000, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
    }

    #[test]
    fn speech_then_trailing_silence_ends_once() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        assert!(
            push(&mut d, -30.0, 13, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        // 30 chunks of silence are not enough (960 ms)...
        assert!(
            push(&mut d, -60.0, 30, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        // ...the 31st ends the phrase, exactly once.
        assert_eq!(d.push(-60.0, true), Step::End);
        assert!(
            push(&mut d, -60.0, 100, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        assert!(
            push(&mut d, -30.0, 100, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
    }

    #[test]
    fn ordinary_pauses_do_not_end() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        push(&mut d, -30.0, 8, true);
        // 20 chunks = 640 ms pause inside the sentence.
        assert!(
            push(&mut d, -60.0, 20, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        push(&mut d, -30.0, 5, true);
        assert!(
            push(&mut d, -60.0, 30, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        assert_eq!(d.push(-60.0, true), Step::End);
    }

    #[test]
    fn wake_tail_in_preroll_does_not_end_the_wait() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        // The Ricardo pre-roll ends at the confirmation: this is the wake tail.
        push(&mut d, -30.0, 12, false);
        // No live speech follows: never end, keep waiting for a command.
        assert!(
            push(&mut d, -60.0, 500, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
    }

    #[test]
    fn command_continuous_from_preroll_ends() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        // Speech started before the wake word was handled...
        push(&mut d, -30.0, 6, false);
        // ...and continues in the live stream for 160 ms.
        push(&mut d, -30.0, 5, true);
        assert!(
            push(&mut d, -60.0, 30, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        assert_eq!(d.push(-60.0, true), Step::End);
    }

    #[test]
    fn exactly_five_live_chunks_qualify_when_speech_starts_in_live_audio() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        // Five live chunks cross the onset threshold *and* are the live evidence.
        assert!(
            push(&mut d, -30.0, 5, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        assert!(
            push(&mut d, -60.0, 30, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        assert_eq!(d.push(-60.0, true), Step::End);
    }

    #[test]
    fn four_live_chunks_do_not_qualify() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        push(&mut d, -30.0, 4, true);
        assert!(
            push(&mut d, -60.0, 100, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
    }

    #[test]
    fn preroll_arms_speech_but_does_not_count_as_live_evidence() {
        // Five pre-roll chunks open Speech; only two live chunks follow.
        let mut d = EndpointDetector::new(prior()).unwrap();
        push(&mut d, -30.0, 5, false);
        push(&mut d, -30.0, 2, true);
        // Three more live chunks reach exactly five consecutive live frames.
        push(&mut d, -30.0, 3, true);
        assert!(
            push(&mut d, -60.0, 30, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
        assert_eq!(d.push(-60.0, true), Step::End);

        // The same mixed start with only four live chunks stays open.
        let mut d = EndpointDetector::new(prior()).unwrap();
        push(&mut d, -30.0, 5, false);
        push(&mut d, -30.0, 4, true);
        assert!(
            push(&mut d, -60.0, 100, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
    }

    #[test]
    fn short_live_wake_tail_does_not_count_as_a_command() {
        let mut d = EndpointDetector::new(prior()).unwrap();
        push(&mut d, -30.0, 12, false);
        // The tail continues into live audio for 4 chunks (128 ms) and then stops.
        push(&mut d, -30.0, 4, true);
        assert!(
            push(&mut d, -60.0, 500, true)
                .iter()
                .all(|s| *s == Step::Continue)
        );
    }

    #[test]
    fn pcm_speech_and_silence_end_the_phrase() {
        let speech: Vec<i16> = vec![3000; 512];
        let silence: Vec<i16> = vec![20; 512];
        let n = pcm_level_db(&speech);
        let f = pcm_level_db(&silence);
        assert!(n > f + MIN_PRIOR_RANGE_DB, "{n} {f}");
        let mut d = EndpointDetector::new(EndpointPrior {
            noise_db: f,
            speech_db: n,
        })
        .unwrap();
        for _ in 0..13 {
            assert_eq!(d.push(pcm_level_db(&speech), true), Step::Continue);
        }
        let mut end = 0;
        for _ in 0..40 {
            if d.push(pcm_level_db(&silence), true) == Step::End {
                end += 1;
            }
        }
        assert_eq!(end, 1);
    }

    fn run_with_phrase() -> EndpointRun {
        let mut r = EndpointRun::new(prior(), &[], &[]).unwrap();
        for _ in 0..13 {
            r.push(-30.0, true);
        }
        for _ in 0..31 {
            r.push(-60.0, true);
        }
        r
    }

    #[test]
    fn one_graceful_end_per_dialog() {
        let mut r = run_with_phrase();
        assert!(r.take_end());
        assert!(!r.take_end());
        for _ in 0..50 {
            r.push(-60.0, true);
            assert!(!r.take_end());
        }
    }

    #[test]
    fn ha_finishing_first_cancels_the_detector() {
        let mut r = EndpointRun::new(prior(), &[], &[]).unwrap();
        for _ in 0..13 {
            r.push(-30.0, true);
        }
        for _ in 0..31 {
            r.push(-60.0, true);
        }
        // STT_VAD_END/STT_END arrived before the controller flushed the end.
        r.cancel();
        assert!(!r.take_end());
    }

    #[test]
    fn cancel_and_repeated_wake_cannot_send_the_old_end() {
        let mut r = EndpointRun::new(prior(), &[], &[]).unwrap();
        for _ in 0..13 {
            r.push(-30.0, true);
        }
        for _ in 0..31 {
            r.push(-60.0, true);
        }
        assert!(r.has_pending());
        r.cancel();
        assert!(!r.take_end());
        // A repeated wake started a fresh run; the old one is gone with its state.
        let mut next = EndpointRun::new(prior(), &[], &[]).unwrap();
        assert!(!next.take_end());
    }

    #[test]
    fn speech_resuming_during_the_earcon_drops_the_deferred_end_but_can_end_later() {
        let mut r = EndpointRun::new(prior(), &[], &[]).unwrap();
        for _ in 0..13 {
            r.push(-30.0, true);
        }
        for _ in 0..31 {
            r.push(-60.0, true);
        }
        assert!(r.has_pending());
        // The user kept talking over the earcon: this was not the end.
        r.push(-30.0, true);
        assert!(!r.has_pending());
        assert!(!r.take_end());
        // ...but the next silence still ends the same dialog.
        for _ in 0..31 {
            r.push(-60.0, true);
        }
        assert!(r.take_end());
    }

    #[test]
    fn end_found_in_the_arbitration_backlog_is_not_lost() {
        // 13 speech + 31 silence chunks = 1.4 s of live audio, more than the 250 ms window but
        // possible when the scheduler was delayed.
        let mut live = vec![3000i16; 13 * 512];
        live.extend(std::iter::repeat_n(20i16, 31 * 512));
        let speech = pcm_level_db(&live[..512]);
        let floor = pcm_level_db(&live[13 * 512..14 * 512]);
        let prior = EndpointPrior {
            noise_db: floor,
            speech_db: speech,
        };
        let mut r = EndpointRun::new(prior, &[], &live).unwrap();
        assert!(r.has_pending());
        assert!(r.take_end());
    }

    #[test]
    fn quiet_part_of_the_window_is_the_percentile() {
        // 40 quiet chunks and 7 loud ones: the 10th percentile is the floor.
        let mut w = vec![1e-6; 40];
        w.extend_from_slice(&[1e-2; 7]);
        assert!((percentile_db(&w, 0.10) - (-60.0)).abs() < 0.1);
        assert!((percentile_db(&w, 0.90) - (-20.0)).abs() < 0.1);
    }
}
