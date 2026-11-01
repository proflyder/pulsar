//! Pure decisions of wake-event recording: ids and UTC text, the model manifest thresholds, the
//! WAV container, sidecar and stats JSON, retention and the near-miss peak. No I/O here, so every
//! rule of the contract is unit-tested next to it.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::json::{self, Json};

pub const SAMPLE_RATE: u32 = 16000;
pub const DEFAULT_CUTOFF: f64 = 0.9;
pub const DEFAULT_WINDOW: usize = 10;
/// Cheap near-miss band: a local maximum of the windowed mean below the cutoff counts from here.
pub const NEAR_THRESHOLD: f32 = 0.5;
/// Two near-misses are at least this far apart.
pub const NEAR_GAP_MS: i64 = 3000;
/// No near-miss this soon after a confirmed hit.
pub const NEAR_AFTER_HIT_MS: i64 = 2000;
/// «Wake: ложное срабатывание» labels the newest hit of this window.
pub const FALSE_ALARM_WINDOW_MS: i64 = 10 * 60 * 1000;
/// Retention caps: 300 MB or 3000 events.
pub const MAX_BYTES: u64 = 300 * 1024 * 1024;
pub const MAX_EVENTS: usize = 3000;

/// Event kinds of contract v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hit,
    Near,
    Miss,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Hit => "hit",
            Kind::Near => "near",
            Kind::Miss => "miss",
        }
    }
}

/// Outcome of a hit's dialog, filled by the controller when the dialog ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Answered,
    EmptyStt,
    Cancelled,
    Error,
    DryRun,
    ArbiterLost,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Answered => "answered",
            Outcome::EmptyStt => "empty_stt",
            Outcome::Cancelled => "cancelled",
            Outcome::Error => "error",
            Outcome::DryRun => "dry_run",
            Outcome::ArbiterLost => "arbiter_lost",
        }
    }
}

/// `micro.probability_cutoff` and `micro.sliding_window_size` of the model manifest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub cutoff: f32,
    pub window: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            cutoff: DEFAULT_CUTOFF as f32,
            window: DEFAULT_WINDOW,
        }
    }
}

/// The manifest next to the loaded `.tflite` (same stem, `.json`); None when it is missing or its
/// values are outside the contract's ranges, so the loaded defaults stay.
pub fn manifest_thresholds(text: &str) -> Option<Thresholds> {
    let doc = json::parse(text)?;
    let micro = doc.get("micro")?;
    let cutoff = micro.get("probability_cutoff")?.as_f64()?;
    let window = micro.get("sliding_window_size")?.as_u64()?;
    ((cutoff > 0.0 && cutoff < 1.0) && (1..=50).contains(&window)).then_some(Thresholds {
        cutoff: cutoff as f32,
        window: window as usize,
    })
}

/// Reads the thresholds for `model`; the bool tells whether the manifest was used.
pub fn thresholds_for(model: &Path) -> (Thresholds, bool) {
    let path = model.with_extension("json");
    match std::fs::read_to_string(&path) {
        Ok(text) => match manifest_thresholds(&text) {
            Some(t) => (t, true),
            None => (Thresholds::default(), false),
        },
        Err(_) => (Thresholds::default(), false),
    }
}

/// Milliseconds since the Unix epoch (wall clock, for ids and sidecars; the audio path uses the
/// monotonic `pulsar_core::now_ms` for its intervals).
pub fn utc_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_utc(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// `YYYYMMDDTHHMMSSZ`, the id prefix.
pub fn format_id_time(ms: i64) -> String {
    let (y, mo, d, h, mi, s, _) = civil(ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `<UTC yyyymmddTHHMMSSZ>-<kind>-<seq>`, `seq` a per-boot counter.
pub fn event_id(ms: i64, kind: Kind, seq: u64) -> String {
    format!("{}-{}-{seq}", format_id_time(ms), kind.as_str())
}

/// Parses `format_utc` back; used for retention order and the false-alarm window.
pub fn parse_utc(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(b.get(r)?).ok()?.parse().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let milli = if b.get(19) == Some(&b'.') {
        num(20..23)?
    } else {
        0
    };
    let days = days_from_civil(y as i32, mo as u32, d as u32);
    Some((((days * 24 + h) * 60 + mi) * 60 + s) * 1000 + milli)
}

/// (year, month, day, hour, minute, second, millisecond) in UTC.
fn civil(ms: i64) -> (i32, u32, u32, u32, u32, u32, u32) {
    let ms = ms.max(0);
    let secs = ms / 1000;
    let milli = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        (rem / 3600) as u32,
        (rem / 60 % 60) as u32,
        (rem % 60) as u32,
        milli,
    )
}

/// Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    ((if m <= 2 { y + 1 } else { y }) as i32, m as u32, d as u32)
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(if m <= 2 { y - 1 } else { y });
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One WAV chunk of PCM16 mono 16 kHz: a 44-byte canonical header and the samples.
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data);
    let u32le = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    let u16le = |out: &mut Vec<u8>, v: u16| out.extend_from_slice(&v.to_le_bytes());
    out.extend_from_slice(b"RIFF");
    u32le(&mut out, (36 + data) as u32);
    out.extend_from_slice(b"WAVEfmt ");
    u32le(&mut out, 16); // fmt chunk size
    u16le(&mut out, 1); // PCM
    u16le(&mut out, 1); // mono
    u32le(&mut out, SAMPLE_RATE);
    u32le(&mut out, SAMPLE_RATE * 2); // byte rate
    u16le(&mut out, 2); // block align
    u16le(&mut out, 16); // bits per sample
    out.extend_from_slice(b"data");
    u32le(&mut out, data as u32);
    for s in samples {
        u16le(&mut out, *s as u16);
    }
    out
}

/// Events of one long listening period, loaded from `stats.json` and extended by the counters of
/// this run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub since_utc: String,
    pub listening_seconds: f64,
    pub hits: u64,
    pub near: u64,
    pub miss: u64,
    pub dry_run_listening_seconds: f64,
    pub dropped: u64,
}

fn num(v: f64) -> String {
    // Enough digits for the accumulators and the thresholds, without an exponent for 0.9 / 0.934.
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".into()
    } else {
        s.into()
    }
}

fn num3(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

pub fn stats_json(s: &Stats, model_sha256: &str) -> String {
    format!(
        "{{\"schema\":1,\"since_utc\":{},\"listening_seconds\":{},\"hits\":{},\"near\":{},\
         \"miss\":{},\"dry_run_listening_seconds\":{},\"dropped\":{},\"model_sha256\":{}}}\n",
        json::escape(&s.since_utc),
        num(s.listening_seconds),
        s.hits,
        s.near,
        s.miss,
        num(s.dry_run_listening_seconds),
        s.dropped,
        json::escape(model_sha256),
    )
}

pub fn parse_stats(text: &str) -> Option<Stats> {
    let doc = json::parse(text)?;
    Some(Stats {
        since_utc: doc.get("since_utc")?.as_str()?.to_owned(),
        listening_seconds: doc.get("listening_seconds").and_then(Json::as_f64)?,
        hits: doc.get("hits").and_then(Json::as_u64).unwrap_or(0),
        near: doc.get("near").and_then(Json::as_u64).unwrap_or(0),
        miss: doc.get("miss").and_then(Json::as_u64).unwrap_or(0),
        dry_run_listening_seconds: doc
            .get("dry_run_listening_seconds")
            .and_then(Json::as_f64)
            .unwrap_or(0.0),
        dropped: doc.get("dropped").and_then(Json::as_u64).unwrap_or(0),
    })
}

/// `<id>.json`, the sidecar of contract v1.
#[derive(Clone, Debug, PartialEq)]
pub struct Sidecar {
    pub schema: u32,
    pub id: String,
    pub kind: String,
    pub utc: String,
    pub sample_rate: u32,
    pub pre_ms: u32,
    pub post_ms: u32,
    pub score_peak: f64,
    pub cutoff: f64,
    pub window: u32,
    pub model_sha256: String,
    pub pulsar_version: String,
    pub dry_run: bool,
    pub outcome: Option<String>,
    pub stt_text: Option<String>,
    pub label: Option<String>,
    pub label_source: Option<String>,
    pub labeled_utc: Option<String>,
}

fn json_opt(v: &Option<String>) -> String {
    v.as_deref().map_or_else(|| "null".into(), json::escape)
}

pub fn sidecar_json(s: &Sidecar) -> String {
    format!(
        "{{\"schema\":{},\"id\":{},\"kind\":{},\"utc\":{},\"sample_rate\":{},\"pre_ms\":{},\
         \"post_ms\":{},\"score_peak\":{},\"cutoff\":{},\"window\":{},\"model_sha256\":{},\
         \"pulsar_version\":{},\"dry_run\":{},\"outcome\":{},\"stt_text\":{},\"label\":{},\
         \"label_source\":{},\"labeled_utc\":{}}}\n",
        s.schema,
        json::escape(&s.id),
        json::escape(&s.kind),
        json::escape(&s.utc),
        s.sample_rate,
        s.pre_ms,
        s.post_ms,
        num3(s.score_peak),
        num(s.cutoff),
        s.window,
        json::escape(&s.model_sha256),
        json::escape(&s.pulsar_version),
        s.dry_run,
        json_opt(&s.outcome),
        json_opt(&s.stt_text),
        json_opt(&s.label),
        json_opt(&s.label_source),
        json_opt(&s.labeled_utc),
    )
}

pub fn parse_sidecar(text: &str) -> Option<Sidecar> {
    let doc = json::parse(text)?;
    let opt = |k: &str| doc.get(k).and_then(Json::as_str).map(str::to_owned);
    Some(Sidecar {
        schema: doc.get("schema").and_then(Json::as_u64).unwrap_or(1) as u32,
        id: opt("id")?,
        kind: opt("kind")?,
        utc: opt("utc")?,
        sample_rate: doc
            .get("sample_rate")
            .and_then(Json::as_u64)
            .unwrap_or(16000) as u32,
        pre_ms: doc.get("pre_ms").and_then(Json::as_u64).unwrap_or(0) as u32,
        post_ms: doc.get("post_ms").and_then(Json::as_u64).unwrap_or(0) as u32,
        score_peak: doc.get("score_peak").and_then(Json::as_f64).unwrap_or(0.0),
        cutoff: doc.get("cutoff").and_then(Json::as_f64).unwrap_or(0.0),
        window: doc.get("window").and_then(Json::as_u64).unwrap_or(0) as u32,
        model_sha256: opt("model_sha256").unwrap_or_default(),
        pulsar_version: opt("pulsar_version").unwrap_or_default(),
        dry_run: doc.get("dry_run").and_then(Json::as_bool).unwrap_or(false),
        outcome: opt("outcome"),
        stt_text: opt("stt_text"),
        label: opt("label"),
        label_source: opt("label_source"),
        labeled_utc: opt("labeled_utc"),
    })
}

/// What retention and the Ha buttons need from a sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    pub kind: String,
    pub utc_ms: i64,
    pub labeled: bool,
    /// WAV + sidecar size.
    pub bytes: u64,
}

/// Indices to delete, oldest first: unlabeled `near`, then unlabeled `hit`, then anything. Stops
/// as soon as both caps hold.
pub fn retention_victims(
    events: &[Meta],
    total_bytes: u64,
    max_bytes: u64,
    max_events: usize,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_by(|&a, &b| {
        events[a]
            .utc_ms
            .cmp(&events[b].utc_ms)
            .then_with(|| events[a].id.cmp(&events[b].id))
    });
    let mut removed = vec![false; events.len()];
    let (mut count, mut bytes) = (events.len(), total_bytes);
    let mut victims = Vec::new();
    while count > max_events || bytes > max_bytes {
        let pick = order
            .iter()
            .copied()
            .find(|&i| !removed[i] && events[i].kind == "near" && !events[i].labeled)
            .or_else(|| {
                order
                    .iter()
                    .copied()
                    .find(|&i| !removed[i] && events[i].kind == "hit" && !events[i].labeled)
            })
            .or_else(|| order.iter().copied().find(|&i| !removed[i]));
        let Some(i) = pick else { break };
        removed[i] = true;
        count -= 1;
        bytes = bytes.saturating_sub(events[i].bytes);
        victims.push(i);
    }
    victims
}

/// Index of the newest hit within `FALSE_ALARM_WINDOW_MS` of `now_ms`.
pub fn false_alarm_target(events: &[Meta], now_ms: i64) -> Option<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.kind == "hit" && now_ms.saturating_sub(e.utc_ms) <= FALSE_ALARM_WINDOW_MS
        })
        .max_by(|(_, a), (_, b)| a.utc_ms.cmp(&b.utc_ms).then_with(|| a.id.cmp(&b.id)))
        .map(|(i, _)| i)
}

/// A local maximum of the windowed mean that stays below the cutoff: `prev` is above what came
/// before it and is not below the current chunk.
pub fn near_peak(prev2: Option<f32>, prev: f32, cur: f32, cutoff: f32) -> bool {
    let before = prev2.unwrap_or(-1.0);
    prev > before && prev >= cur && prev >= NEAR_THRESHOLD && prev < cutoff
}

/// Whole VQE chunks covering at least `ms` of 16 kHz audio.
pub fn chunks_for(ms: u64, frame: usize) -> usize {
    let samples = (u64::from(SAMPLE_RATE) * ms / 1000) as usize;
    samples.div_ceil(frame.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_text_round_trips() {
        // 2026-11-01T10:15:00.123Z
        let ms = parse_utc("2026-11-01T10:15:00.123Z").unwrap();
        assert_eq!(format_utc(ms), "2026-11-01T10:15:00.123Z");
        assert_eq!(format_id_time(ms), "20261101T101500Z");
        assert_eq!(event_id(ms, Kind::Hit, 17), "20261101T101500Z-hit-17");
        assert_eq!(parse_utc("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_utc("2024-02-29T23:59:59.999Z")
                .map(format_utc)
                .as_deref(),
            Some("2024-02-29T23:59:59.999Z")
        );
        // Without milliseconds the same instant; malformed text is rejected.
        assert_eq!(parse_utc("2026-11-01T10:15:00Z"), Some(ms / 1000 * 1000));
        for bad in ["", "2026-11-01", "x026-11-01T10:15:00.123Z"] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
        assert_eq!(format_id_time(ms).len(), 16);
    }

    #[test]
    fn manifest_values_are_validated() {
        let ok = r#"{"micro":{"probability_cutoff":0.87,"sliding_window_size":12}}"#;
        assert_eq!(
            manifest_thresholds(ok),
            Some(Thresholds {
                cutoff: 0.87,
                window: 12
            })
        );
        for bad in [
            r#"{"micro":{"probability_cutoff":0,"sliding_window_size":12}}"#,
            r#"{"micro":{"probability_cutoff":1,"sliding_window_size":12}}"#,
            r#"{"micro":{"probability_cutoff":0.9,"sliding_window_size":0}}"#,
            r#"{"micro":{"probability_cutoff":0.9,"sliding_window_size":51}}"#,
            r#"{"micro":{"probability_cutoff":0.9}}"#,
            r#"{"other":{}}"#,
            "not json",
        ] {
            assert_eq!(manifest_thresholds(bad), None, "{bad}");
        }
        assert_eq!(Thresholds::default().window, 10);
    }

    #[test]
    fn wav_header_is_canonical_pcm16_mono_16k() {
        let w = wav(&[0, 1, -1, i16::MAX]);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(w[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(w[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16000);
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 32000);
        assert_eq!(u16::from_le_bytes(w[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(w[34..36].try_into().unwrap()), 16);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 8);
        assert_eq!(&w[44..], &[0, 0, 1, 0, 0xff, 0xff, 0xff, 0x7f]);
    }

    #[test]
    fn stats_and_sidecars_round_trip() {
        let stats = Stats {
            since_utc: "2026-11-01T10:00:00.000Z".into(),
            listening_seconds: 12345.6,
            hits: 10,
            near: 40,
            miss: 2,
            dry_run_listening_seconds: 3600.0,
            dropped: 0,
        };
        let text = stats_json(&stats, "abc");
        assert_eq!(parse_stats(&text), Some(stats));
        assert!(text.ends_with("}\n"));

        let side = Sidecar {
            schema: 1,
            id: "20261101T101500Z-hit-17".into(),
            kind: "hit".into(),
            utc: "2026-11-01T10:15:00.123Z".into(),
            sample_rate: 16000,
            pre_ms: 3000,
            post_ms: 1000,
            score_peak: 0.934,
            cutoff: 0.9,
            window: 10,
            model_sha256: "deadbeef".into(),
            pulsar_version: "v0.5.1003".into(),
            dry_run: false,
            outcome: None,
            stt_text: None,
            label: None,
            label_source: None,
            labeled_utc: None,
        };
        let text = sidecar_json(&side);
        assert_eq!(parse_sidecar(&text), Some(side.clone()));
        assert!(text.contains("\"outcome\":null"));
        assert!(text.contains("\"score_peak\":0.934"));
        let labeled = Sidecar {
            outcome: Some("answered".into()),
            stt_text: Some("".into()),
            label: Some("false".into()),
            label_source: Some("user".into()),
            labeled_utc: Some("2026-11-01T10:20:00.000Z".into()),
            ..side
        };
        assert_eq!(parse_sidecar(&sidecar_json(&labeled)), Some(labeled));
    }

    fn meta(id: &str, kind: &str, utc_ms: i64, labeled: bool, bytes: u64) -> Meta {
        Meta {
            id: id.into(),
            kind: kind.into(),
            utc_ms,
            labeled,
            bytes,
        }
    }

    #[test]
    fn retention_prefers_unlabeled_near_then_hit() {
        let e = vec![
            meta("a", "hit", 1, false, 100),
            meta("b", "near", 2, false, 100),
            meta("c", "near", 3, true, 100),
            meta("d", "hit", 4, false, 100),
            meta("e", "miss", 5, true, 100),
        ];
        // Two over: the oldest unlabeled near and the next one (labeled near comes later).
        assert_eq!(retention_victims(&e, 500, 300, 10), vec![1, 0]);
        // Only the count is over: oldest unlabeled near / hit first, then anything.
        assert_eq!(retention_victims(&e, 500, 10_000, 3), vec![1, 0]);
        // Nothing over.
        assert!(retention_victims(&e, 500, 500, 5).is_empty());
        // All labeled: oldest overall first.
        let all = vec![
            meta("x", "miss", 1, true, 10),
            meta("y", "hit", 2, true, 10),
        ];
        assert_eq!(retention_victims(&all, 20, 10, 10), vec![0]);
    }

    #[test]
    fn false_alarm_is_the_newest_recent_hit() {
        let now = 1_000_000_000;
        let e = vec![
            meta("old", "hit", now - FALSE_ALARM_WINDOW_MS - 1, false, 1),
            meta("h1", "hit", now - 60_000, false, 1),
            meta("h2", "hit", now - 1000, false, 1),
            meta("n", "near", now - 500, false, 1),
        ];
        assert_eq!(false_alarm_target(&e, now), Some(2));
        assert_eq!(false_alarm_target(&e[..1], now), None);
        assert_eq!(false_alarm_target(&[], now), None);
    }

    #[test]
    fn near_peak_needs_a_fall_below_the_window() {
        // Rise, peak 0.7, fall: a near-miss.
        assert!(near_peak(Some(0.4), 0.7, 0.6, 0.9));
        // Still rising or flat: not yet a peak.
        assert!(!near_peak(Some(0.4), 0.6, 0.7, 0.9));
        assert!(near_peak(Some(0.4), 0.7, 0.7, 0.9));
        // Below the near band, at or above the cutoff: no near event.
        assert!(!near_peak(Some(0.1), 0.4, 0.2, 0.9));
        assert!(!near_peak(Some(0.5), 0.95, 0.3, 0.9));
        // First windowed chunk: nothing before it, so not a local maximum.
        assert!(near_peak(None, 0.7, 0.6, 0.9));
    }

    #[test]
    fn window_chunks_cover_at_least_the_window() {
        assert_eq!(chunks_for(3000, 512), 94); // 3008 ms
        assert_eq!(chunks_for(1000, 512), 32); // 1024 ms
        assert_eq!(chunks_for(6000, 512), 188); // 6016 ms
        assert_eq!(chunks_for(3000, 256), 188); // 3008 ms, Mini 3 / Mini chunks
    }
}
