//! The wake-event writer thread: WAV + sidecar files, `stats.json`, the in-memory index, retention
//! and the labels the controller asks for. All blocking file work of the feature lives here, off
//! the audio thread.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{debug, info, warn};

use super::logic::{self, MAX_BYTES, MAX_EVENTS, Outcome, Sidecar, Stats, Thresholds};
use super::{Command, Counters, Job, Recording};

/// The writer wakes up this often to look for commands and the 60 s stats period.
const WAKE_MS: u64 = 200;
/// Outcomes may arrive before the window they belong to (a lost arbitration ~250 ms after the
/// hit, the audio 1 s later): they wait here for its sidecar.
const MAX_PENDING_OUTCOMES: usize = 32;

pub(super) fn spawn(
    dir: PathBuf,
    version: String,
    sha256: String,
    thresholds: Thresholds,
    counters: Arc<Counters>,
    cmd: Receiver<Command>,
    jobs: Receiver<Job>,
) -> Option<JoinHandle<()>> {
    let writer = Writer {
        dir,
        version,
        sha256,
        thresholds,
        counters,
        cmd,
        jobs,
        base: Stats::default(),
        index: Vec::new(),
        last_stats_ms: pulsar_core::now_ms(),
        last_written: None,
        pending_outcomes: Vec::new(),
    };
    match std::thread::Builder::new()
        .name("wake-events".into())
        .spawn(move || writer.run())
    {
        Ok(t) => Some(t),
        Err(e) => {
            warn!("wake events: cannot start the writer thread: {e}");
            None
        }
    }
}

struct Writer {
    dir: PathBuf,
    version: String,
    sha256: String,
    thresholds: Thresholds,
    counters: Arc<Counters>,
    cmd: Receiver<Command>,
    jobs: Receiver<Job>,
    /// `stats.json` as loaded: this run's counters are added on top.
    base: Stats,
    index: Vec<logic::Meta>,
    last_stats_ms: i64,
    last_written: Option<Stats>,
    /// Outcomes whose sidecar did not exist yet.
    pending_outcomes: Vec<(String, Outcome, Option<String>)>,
}

fn json_path(dir: &Path, id: &str, ext: &str) -> PathBuf {
    dir.join(format!("{id}.{ext}"))
}

/// Write to `<path>.tmp`, fsync, rename, fsync the directory (the pattern of `state.conf`; the
/// Lite 2's ubifs needs it or a power cut leaves an empty file).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = OsString::from(path.as_os_str());
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

impl Writer {
    fn run(mut self) {
        self.load();
        let mut cmd_open = true;
        loop {
            if cmd_open {
                loop {
                    match self.cmd.try_recv() {
                        Ok(c) => self.on_command(c),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            cmd_open = false;
                            break;
                        }
                    }
                }
            }
            match self.jobs.recv_timeout(Duration::from_millis(WAKE_MS)) {
                Ok(Job::Save(rec)) => self.save(*rec),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let due = pulsar_core::now_ms().saturating_sub(self.last_stats_ms) >= 60_000;
            if due {
                self.write_stats();
                self.last_stats_ms = pulsar_core::now_ms();
            }
        }
        // Clean shutdown: the totals of this run reach the file even inside the 60 s period.
        self.write_stats();
    }

    fn load(&mut self) {
        let path = self.dir.join("stats.json");
        self.base = match fs::read_to_string(&path) {
            Ok(text) => match logic::parse_stats(&text) {
                Some(s) => s,
                None => {
                    warn!(
                        "wake events: {} is not valid, counters start over",
                        path.display()
                    );
                    Stats {
                        since_utc: logic::format_utc(logic::utc_ms()),
                        ..Stats::default()
                    }
                }
            },
            Err(_) => Stats {
                since_utc: logic::format_utc(logic::utc_ms()),
                ..Stats::default()
            },
        };
        self.index = scan(&self.dir);
        info!(
            "wake events: {} event(s) on disk, {:.1} listening hour(s) since {}",
            self.index.len(),
            self.base.listening_seconds / 3600.0,
            self.base.since_utc
        );
    }

    fn snapshot(&self) -> Stats {
        let c = &self.counters;
        Stats {
            since_utc: self.base.since_utc.clone(),
            listening_seconds: self.base.listening_seconds
                + c.listening_ms.load(Relaxed) as f64 / 1000.0,
            hits: self.base.hits + c.hits.load(Relaxed),
            near: self.base.near + c.near.load(Relaxed),
            miss: self.base.miss + c.miss.load(Relaxed),
            dry_run_listening_seconds: self.base.dry_run_listening_seconds
                + c.dry_run_ms.load(Relaxed) as f64 / 1000.0,
            dropped: self.base.dropped + c.dropped.load(Relaxed),
        }
    }

    fn write_stats(&mut self) {
        let stats = self.snapshot();
        if self.last_written.as_ref() == Some(&stats) {
            return;
        }
        let path = self.dir.join("stats.json");
        if let Err(e) = write_atomic(&path, logic::stats_json(&stats, &self.sha256).as_bytes()) {
            warn!("wake events: cannot write {}: {e}", path.display());
            return;
        }
        self.last_written = Some(stats);
    }

    fn save(&mut self, rec: Recording) {
        let side = Sidecar {
            schema: 1,
            id: rec.id.clone(),
            kind: rec.kind.as_str().to_owned(),
            utc: logic::format_utc(rec.utc_ms),
            sample_rate: logic::SAMPLE_RATE,
            pre_ms: rec.pre_ms,
            post_ms: rec.post_ms,
            score_peak: f64::from(rec.score_peak),
            cutoff: f64::from(self.thresholds.cutoff),
            window: self.thresholds.window as u32,
            model_sha256: self.sha256.clone(),
            pulsar_version: self.version.clone(),
            dry_run: rec.dry_run,
            // A dry-run hit never reaches the controller: its outcome is final here.
            outcome: (rec.kind == super::Kind::Hit && rec.dry_run)
                .then(|| Outcome::DryRun.as_str().to_owned()),
            stt_text: None,
            label: rec.label.map(str::to_owned),
            label_source: rec.label_source.map(str::to_owned),
            labeled_utc: rec.label.map(|_| logic::format_utc(logic::utc_ms())),
        };
        let wav = logic::wav(&rec.samples);
        let json = logic::sidecar_json(&side);
        let wav_path = json_path(&self.dir, &rec.id, "wav");
        let json_path = json_path(&self.dir, &rec.id, "json");
        if let Err(e) =
            write_atomic(&wav_path, &wav).and_then(|()| write_atomic(&json_path, json.as_bytes()))
        {
            warn!("wake events: cannot write {}: {e}", wav_path.display());
            let _ = fs::remove_file(&wav_path);
            return;
        }
        info!(
            "wake events: {} {} ({:.1} s, peak {:.3})",
            rec.id,
            rec.kind.as_str(),
            rec.samples.len() as f64 / f64::from(logic::SAMPLE_RATE),
            rec.score_peak
        );
        self.index.push(logic::Meta {
            id: rec.id.clone(),
            kind: rec.kind.as_str().to_owned(),
            utc_ms: rec.utc_ms,
            labeled: side.label.is_some(),
            bytes: (wav.len() + json.len()) as u64,
        });
        self.enforce_retention();
        if let Some(i) = self.pending_outcomes.iter().position(|p| p.0 == rec.id) {
            let (_, outcome, stt_text) = self.pending_outcomes.remove(i);
            self.apply_outcome(&rec.id, outcome, stt_text.as_deref());
        }
    }

    fn enforce_retention(&mut self) {
        let total: u64 = self.index.iter().map(|m| m.bytes).sum();
        let victims = logic::retention_victims(&self.index, total, MAX_BYTES, MAX_EVENTS);
        for i in victims.into_iter().rev() {
            let meta = self.index.remove(i);
            for ext in ["wav", "json"] {
                let _ = fs::remove_file(json_path(&self.dir, &meta.id, ext));
            }
            debug!("wake events: retention removed {}", meta.id);
        }
    }

    fn on_command(&mut self, c: Command) {
        match c {
            Command::Outcome {
                id,
                outcome,
                stt_text,
            } => {
                if self.read_sidecar(&id).is_none() {
                    // The hit's window finishes 1 s after the confirming chunk; an outcome of a
                    // dialog that never started (arbiter_lost, a quick cancel) comes earlier.
                    if self.pending_outcomes.len() >= MAX_PENDING_OUTCOMES {
                        self.pending_outcomes.remove(0);
                    }
                    self.pending_outcomes.push((id, outcome, stt_text));
                    return;
                }
                self.apply_outcome(&id, outcome, stt_text.as_deref());
            }
            Command::FalseAlarm => {
                let now = logic::utc_ms();
                let picked = logic::false_alarm_target(&self.index, now);
                let Some(i) = picked else {
                    warn!("wake events: no hit in the last 10 minutes to label");
                    return;
                };
                let id = self.index[i].id.clone();
                let Some(mut side) = self.read_sidecar(&id) else {
                    warn!("wake events: sidecar of {id} is missing or invalid");
                    return;
                };
                side.label = Some("false".into());
                side.label_source = Some("user".into());
                side.labeled_utc = Some(logic::format_utc(now));
                if let Err(e) = write_atomic(
                    &json_path(&self.dir, &id, "json"),
                    logic::sidecar_json(&side).as_bytes(),
                ) {
                    warn!("wake events: cannot label {id}: {e}");
                    return;
                }
                self.index[i].labeled = true;
                info!("wake events: {id} labeled false by the user");
            }
        }
    }

    fn apply_outcome(&mut self, id: &str, outcome: Outcome, stt_text: Option<&str>) {
        let Some(mut side) = self.read_sidecar(id) else {
            debug!("wake events: no sidecar for {id}");
            return;
        };
        side.outcome = Some(outcome.as_str().to_owned());
        if let Some(t) = stt_text {
            side.stt_text = Some(t.to_owned());
        }
        if let Err(e) = write_atomic(
            &json_path(&self.dir, id, "json"),
            logic::sidecar_json(&side).as_bytes(),
        ) {
            warn!("wake events: cannot update {id}: {e}");
        } else {
            debug!("wake events: {id} outcome {}", outcome.as_str());
        }
    }

    fn read_sidecar(&self, id: &str) -> Option<Sidecar> {
        let text = fs::read_to_string(json_path(&self.dir, id, "json")).ok()?;
        logic::parse_sidecar(&text)
    }
}

/// The events already on disk (a previous run), for retention and the false-alarm window.
fn scan(dir: &Path) -> Vec<logic::Meta> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if id == "stats" {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(side) = logic::parse_sidecar(&text) else {
            continue;
        };
        let wav = path.with_extension("wav").metadata().map_or(0, |m| m.len());
        out.push(logic::Meta {
            id: side.id,
            kind: side.kind,
            utc_ms: logic::parse_utc(&side.utc).unwrap_or(0),
            labeled: side.label.is_some(),
            bytes: text.len() as u64 + wav,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake_events::Kind;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "pulsar-wake-events-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn writer(dir: &Path) -> (Writer, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        let (_cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (_job_tx, job_rx) = std::sync::mpsc::sync_channel(1);
        (
            Writer {
                dir: dir.to_owned(),
                version: "v0.5.1003".into(),
                sha256: "abc".into(),
                thresholds: Thresholds::default(),
                counters: counters.clone(),
                cmd: cmd_rx,
                jobs: job_rx,
                base: Stats::default(),
                index: Vec::new(),
                last_stats_ms: 0,
                last_written: None,
                pending_outcomes: Vec::new(),
            },
            counters,
        )
    }

    #[test]
    fn writes_sidecar_wav_and_stats_then_labels_the_newest_hit() {
        let dir = temp_dir("save");
        fs::create_dir_all(&dir).unwrap();
        let (mut w, counters) = writer(&dir);
        counters.listening_ms.fetch_add(1500, Relaxed);
        let now = logic::utc_ms();
        let rec = Recording {
            id: logic::event_id(now, Kind::Hit, 1),
            kind: Kind::Hit,
            utc_ms: now,
            pre_ms: 3000,
            post_ms: 1000,
            score_peak: 0.934,
            dry_run: false,
            samples: vec![0; 16000],
            label: None,
            label_source: None,
        };
        let id = rec.id.clone();
        w.save(rec);
        made(&dir, &id);
        // The sidecar has the contract shape and the WAV is 1 s of PCM16.
        let side = w.read_sidecar(&id).unwrap();
        assert_eq!(side.kind, "hit");
        assert_eq!(side.outcome, None);
        assert_eq!(side.window, 10);
        assert_eq!(
            fs::metadata(dir.join(format!("{id}.wav"))).unwrap().len(),
            44 + 32000
        );
        // Outcome from the controller.
        w.on_command(Command::Outcome {
            id: id.clone(),
            outcome: Outcome::Answered,
            stt_text: Some("включи свет".into()),
        });
        let side = w.read_sidecar(&id).unwrap();
        assert_eq!(side.outcome.as_deref(), Some("answered"));
        assert_eq!(side.stt_text.as_deref(), Some("включи свет"));
        // False alarm labels that hit.
        w.on_command(Command::FalseAlarm);
        let side = w.read_sidecar(&id).unwrap();
        assert_eq!(side.label.as_deref(), Some("false"));
        assert_eq!(side.label_source.as_deref(), Some("user"));
        assert!(side.labeled_utc.is_some());
        assert!(w.index[0].labeled);
        // Stats: 1.5 s of listening, no hit counted by the counters here.
        w.write_stats();
        let stats =
            logic::parse_stats(&fs::read_to_string(dir.join("stats.json")).unwrap()).unwrap();
        assert!((stats.listening_seconds - 1.5).abs() < 1e-9);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_outcome_before_its_window_waits_for_the_sidecar() {
        let dir = temp_dir("pending");
        fs::create_dir_all(&dir).unwrap();
        let (mut w, _) = writer(&dir);
        let now = logic::utc_ms();
        let rec = Recording {
            id: logic::event_id(now, Kind::Hit, 1),
            kind: Kind::Hit,
            utc_ms: now,
            pre_ms: 3000,
            post_ms: 1000,
            score_peak: 0.95,
            dry_run: false,
            samples: vec![0; 160],
            label: None,
            label_source: None,
        };
        let id = rec.id.clone();
        // A lost arbitration arrives before the 1 s post-roll finishes the window.
        w.on_command(Command::Outcome {
            id: id.clone(),
            outcome: Outcome::ArbiterLost,
            stt_text: None,
        });
        assert_eq!(w.pending_outcomes.len(), 1);
        w.save(rec);
        assert!(w.pending_outcomes.is_empty());
        let side = w.read_sidecar(&id).unwrap();
        assert_eq!(side.outcome.as_deref(), Some("arbiter_lost"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_hit_is_already_finished() {
        let dir = temp_dir("dry");
        fs::create_dir_all(&dir).unwrap();
        let (mut w, _) = writer(&dir);
        let now = logic::utc_ms();
        let rec = Recording {
            id: logic::event_id(now, Kind::Hit, 1),
            kind: Kind::Hit,
            utc_ms: now,
            pre_ms: 3000,
            post_ms: 1000,
            score_peak: 0.95,
            dry_run: true,
            samples: vec![0; 160],
            label: None,
            label_source: None,
        };
        let id = rec.id.clone();
        w.save(rec);
        let side = w.read_sidecar(&id).unwrap();
        assert!(side.dry_run);
        assert_eq!(side.outcome.as_deref(), Some("dry_run"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scan_finds_previous_events() {
        let dir = temp_dir("scan");
        fs::create_dir_all(&dir).unwrap();
        let (mut w, _) = writer(&dir);
        let now = logic::utc_ms();
        for (k, kind) in [(1, Kind::Near), (2, Kind::Hit)] {
            w.save(Recording {
                id: logic::event_id(now + k, kind, k as u64),
                kind,
                utc_ms: now + k,
                pre_ms: 3000,
                post_ms: 1000,
                score_peak: 0.6,
                dry_run: false,
                samples: vec![0; 64],
                label: None,
                label_source: None,
            });
        }
        let found = scan(&dir);
        assert_eq!(found.len(), 2);
        assert_eq!(found.iter().filter(|m| m.kind == "hit").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    fn made(dir: &Path, id: &str) {
        assert!(dir.join(format!("{id}.wav")).exists());
        assert!(dir.join(format!("{id}.json")).exists());
        assert!(!dir.join(format!("{id}.json.tmp")).exists());
    }
}
