// Per-(stream_id, host) measured quality cache.
//
// Stores a rolling buffer of recent observations (cap WINDOW=5) for each
// candidate the proxy has ever sampled. Both the bootstrap ffprobe-replacement
// sweep and the per-play TS classifier feed `push()` with a fully-formed
// `Sample` — there is one path into the buffer, no partial records.
//
// The ranker reads `get()` which returns an `Option<MeasuredQuality>` — the
// aggregate of the buffer:
//   - width/height/codec/pix_fmt/color_transfer/fps: most-recent (these are
//     effectively constants per source; a real re-encode is picked up within
//     ~1 sample because the WINDOW evicts oldest)
//   - bitrate_kbps: median of non-None samples (robust to one bad probe)
//
// Persisted as JSON to `data_dir/measured_quality.json`. Atomic flush task
// writes tempfile → rename on a 5 s debounce when the `dirty` flag flips.
// The full buffer is what's persisted — keeps the median signal across
// restarts.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

pub const WINDOW: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SampleSource {
    /// One-shot probe from the bootstrap sweep (server-internal, no /play/).
    Sweep,
    /// Real user play; values accumulated by `PerPlayAccumulator` and
    /// committed when activity quiesces.
    PerPlay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    #[serde(with = "time::serde::rfc3339")]
    pub at: time::OffsetDateTime,
    pub source: SampleSource,
    /// 0 if no video PID was found (would only happen for audio-only TS in
    /// practice; radio doesn't reach this code path).
    pub width: u32,
    pub height: u32,
    pub codec: Option<String>,
    pub pix_fmt: Option<String>,
    pub color_transfer: Option<String>,
    pub framerate: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    /// True when the TS classifier flagged this source as having DVB
    /// subtitles riding the PCR PID — the unstrippable case. Step 7's
    /// `caps_required` derivation aggregates this across a channel's
    /// sources to decide whether to demand the `dvb_safe` client cap.
    /// `None` on older samples that pre-date the field.
    #[serde(default)]
    pub dvb_unsafe: Option<bool>,
    /// Radio sample rate from the first ADTS frame (Step 10). `None` for
    /// TV samples and for radio samples pre-dating Phase 8.
    #[serde(default)]
    pub sample_rate_hz: Option<u32>,
    /// Radio channel count from the first ADTS frame (Step 10). `None`
    /// for TV samples and for radio samples pre-dating Phase 8.
    #[serde(default)]
    pub audio_channels: Option<u8>,
    /// Phase 0 slice-header walker result. `Some(true)` iff at least one
    /// slice referenced more frames than the SPS declared; `Some(false)`
    /// iff every slice fit; `None` if the segment isn't parsable H.264.
    #[serde(default)]
    pub h264_excess_refs: Option<bool>,
}

/// What the ranker sees — the aggregate of a buffer's samples.
#[derive(Debug, Clone, Serialize)]
pub struct MeasuredQuality {
    pub width: u32,
    pub height: u32,
    pub codec: Option<String>,
    pub pix_fmt: Option<String>,
    pub color_transfer: Option<String>,
    pub framerate: Option<f32>,
    /// Median of non-None bitrate samples in the buffer.
    pub bitrate_kbps: Option<u32>,
    /// Most-recent value (same semantics as the other stable fields).
    /// Used by Step 7 to gate the `dvb_safe` cap per channel.
    pub dvb_unsafe: Option<bool>,
    /// Radio sample rate (Hz) from ADTS — Step 10. `None` for TV.
    pub sample_rate_hz: Option<u32>,
    /// Radio channel count from ADTS — Step 10. `None` for TV.
    pub audio_channels: Option<u8>,
    /// Stability-gated H.264 excess-refs state for the variant on this
    /// host. Computed from the rolling sample window per the gate in the
    /// per-variant caps plan: ON (≥1 positive), OFF (≥2 negatives), else
    /// UNKNOWN.
    pub h264_excess_refs_state: ExcessRefsState,
    /// Phase 1: per-variant caps_required materialised from the rolling
    /// window's last decisive sample. Mirrors today's per-kind baseline +
    /// the variant-specific tags from `derive_variant_caps`.
    pub caps_required: Vec<String>,
    pub samples_count: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub measured_at: time::OffsetDateTime,
}

/// Stability-gate result for `h264_excess_refs` per (variant, host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExcessRefsState {
    /// Variant codec is outside the tag's domain (e.g. HEVC). Vacuously decisive.
    NotApplicable,
    /// At least one positive sample in the window.
    On,
    /// Two most-recent decisive samples are negative.
    Off,
    /// Not enough decisive samples to commit yet — fail-closed.
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MeasuredEntry {
    pub samples: VecDeque<Sample>,
}

impl MeasuredEntry {
    pub fn push(&mut self, s: Sample) {
        if self.samples.len() >= WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    /// width/height/codec/pix_fmt/color_transfer/fps: most-recent.
    /// bitrate_kbps: median of non-None samples (so one bad probe washes out).
    pub fn aggregate(&self) -> Option<MeasuredQuality> {
        let last = self.samples.back()?;
        let mut bitrates: Vec<u32> = self
            .samples
            .iter()
            .filter_map(|s| s.bitrate_kbps)
            .collect();
        bitrates.sort_unstable();
        let bitrate_kbps = bitrates.get(bitrates.len() / 2).copied();
        let h264_excess_refs_state = excess_refs_state(&self.samples, last.codec.as_deref());
        let caps_required =
            derive_variant_caps(last, h264_excess_refs_state);
        Some(MeasuredQuality {
            width: last.width,
            height: last.height,
            codec: last.codec.clone(),
            pix_fmt: last.pix_fmt.clone(),
            color_transfer: last.color_transfer.clone(),
            framerate: last.framerate,
            bitrate_kbps,
            dvb_unsafe: last.dvb_unsafe,
            sample_rate_hz: last.sample_rate_hz,
            audio_channels: last.audio_channels,
            h264_excess_refs_state,
            caps_required,
            samples_count: self.samples.len(),
            measured_at: last.at,
        })
    }
}

/// Apply the asymmetric stability gate from the per-variant plan:
/// - N/A if the variant's codec is outside the tag's domain.
/// - ON if any sample in the window is `Some(true)`.
/// - OFF if the two most-recent non-`null` samples are both `Some(false)`.
/// - UNKNOWN otherwise.
pub fn excess_refs_state(
    samples: &VecDeque<Sample>,
    last_codec: Option<&str>,
) -> ExcessRefsState {
    // N/A — variant isn't H.264.
    if let Some(c) = last_codec {
        if c != "h264" {
            return ExcessRefsState::NotApplicable;
        }
    } else {
        // No codec known; fall back to UNKNOWN.
        return ExcessRefsState::Unknown;
    }
    let mut negatives_seen = 0usize;
    for s in samples.iter().rev() {
        match s.h264_excess_refs {
            Some(true) => return ExcessRefsState::On,
            Some(false) => {
                negatives_seen += 1;
                if negatives_seen >= 2 {
                    return ExcessRefsState::Off;
                }
            }
            None => {
                // null vote — skip.
                continue;
            }
        }
    }
    ExcessRefsState::Unknown
}

/// Per-sample variant caps derivation.
///
/// Universal per kind (TV vs Radio) baseline, plus per-variant video codec
/// and decoder-tolerance / DVB-stress tags. The TV baseline preserves
/// `aac` so a single-audio-codec strict client doesn't lose access to TV.
pub fn derive_variant_caps(
    sample: &Sample,
    excess_state: ExcessRefsState,
) -> Vec<String> {
    let is_radio = sample.sample_rate_hz.is_some()
        || matches!(sample.codec.as_deref(), Some("aac")) && sample.width == 0;
    let mut caps: Vec<String> = if is_radio {
        vec!["hls".into(), "live_audio_only_hls".into()]
    } else {
        vec!["hls".into(), "live_video_hls".into(), "aac".into()]
    };
    // Per-variant video codec for TV samples.
    if !is_radio {
        match sample.codec.as_deref() {
            Some("h264") => caps.push("h264".into()),
            Some("hevc") | Some("h265") => {
                caps.push("hevc".into());
                if sample
                    .pix_fmt
                    .as_deref()
                    .map(|p| p.contains("10"))
                    .unwrap_or(false)
                {
                    caps.push("hevc_main10".into());
                }
            }
            _ => {}
        }
    }
    // Plan §5: fail-closed under UNKNOWN. The asymmetric stability gate
    // returns UNKNOWN when there aren't yet two corroborating decisive
    // samples; in that window we keep the cap so strict clients don't
    // get served a variant we can't yet vouch for. Only `Off` (≥2
    // negatives) and `NotApplicable` (variant codec outside the tag's
    // domain) drop the cap.
    let needs_excess_cap = matches!(
        excess_state,
        ExcessRefsState::On | ExcessRefsState::Unknown,
    ) && matches!(sample.codec.as_deref(), Some("h264"));
    if needs_excess_cap {
        caps.push("h264_excess_refs".into());
    }
    if sample.dvb_unsafe == Some(true) {
        caps.push("dvb_safe".into());
    }
    // Deduplicate & sort for stable output.
    caps.sort();
    caps.dedup();
    caps
}

/// Cache key: (upstream stream_id, normalised host root).
/// Per-host because different hosts proxy to different backends with
/// different delivery characteristics — bitrate and success differ.
pub type Key = (u64, String);

/// On-disk format. Stored as a flat list so JSON round-trips cleanly without
/// needing tuple-keyed maps (serde can't deserialise non-string map keys).
#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDiskFormat {
    entries: Vec<OnDiskEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OnDiskEntry {
    stream_id: u64,
    host: String,
    samples: VecDeque<Sample>,
}

pub struct MeasuredStore {
    inner: RwLock<HashMap<Key, MeasuredEntry>>,
    path: PathBuf,
    dirty: AtomicBool,
    /// Monotonic counter bumped on every `push`. Cache consumers (Phase 6's
    /// per-channel `caps_required` cache) read this to detect that the
    /// underlying samples have moved without racing against the disk-flush
    /// task's consumption of `dirty`.
    generation: AtomicU64,
}

impl MeasuredStore {
    /// Load from disk; returns empty store if the file is missing or
    /// corrupted (warned, not fatal — the sweep will rebuild).
    pub fn load_or_empty(path: PathBuf) -> Self {
        let inner = match std::fs::read_to_string(&path) {
            Ok(body) => match serde_json::from_str::<OnDiskFormat>(&body) {
                Ok(f) => {
                    let mut map = HashMap::with_capacity(f.entries.len());
                    for e in f.entries {
                        map.insert((e.stream_id, e.host), MeasuredEntry { samples: e.samples });
                    }
                    map
                }
                Err(e) => {
                    warn!(path = %path.display(), err = %e, "measured store: parse failed, starting empty");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(path = %path.display(), err = %e, "measured store: read failed, starting empty");
                HashMap::new()
            }
        };
        Self {
            inner: RwLock::new(inner),
            path,
            dirty: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    pub fn get(&self, stream_id: u64, host: &str) -> Option<MeasuredQuality> {
        self.inner
            .read()
            .get(&(stream_id, host.to_string()))
            .and_then(|e| e.aggregate())
    }

    pub fn push(&self, stream_id: u64, host: &str, sample: Sample) {
        let mut g = self.inner.write();
        let entry = g.entry((stream_id, host.to_string())).or_default();
        entry.push(sample);
        drop(g);
        self.dirty.store(true, Ordering::Release);
        // Bump the cache generation counter — independent of `dirty`, which
        // the disk-flush task swaps to false on consumption.
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Monotonic generation counter. Phase 6's `caps_required` cache reads
    /// this to decide whether its derived state is stale relative to the
    /// underlying samples.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Timestamp of the most-recent sample for this key, or `None` if
    /// none has been recorded yet. Used by Phase 5's freshness loop to
    /// decide whether a re-probe is due.
    pub fn most_recent_at(&self, stream_id: u64, host: &str) -> Option<time::OffsetDateTime> {
        self.inner
            .read()
            .get(&(stream_id, host.to_string()))
            .and_then(|e| e.samples.back().map(|s| s.at))
    }

    /// Timestamp of the oldest sample still in the rolling window. Used by
    /// `/admin/caps-readiness` to surface the window's lower bound — a
    /// distinct signal from `most_recent_at` (the upper bound).
    pub fn oldest_sample_at(&self, stream_id: u64, host: &str) -> Option<time::OffsetDateTime> {
        self.inner
            .read()
            .get(&(stream_id, host.to_string()))
            .and_then(|e| e.samples.front().map(|s| s.at))
    }

    pub fn has_samples(&self, stream_id: u64, host: &str) -> bool {
        self.inner
            .read()
            .get(&(stream_id, host.to_string()))
            .map(|e| !e.samples.is_empty())
            .unwrap_or(false)
    }

    pub fn snapshot(&self) -> Vec<(Key, MeasuredEntry)> {
        let g = self.inner.read();
        let mut v: Vec<_> = g.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        v.sort_by(|a, b| (a.0 .0, &a.0 .1).cmp(&(b.0 .0, &b.0 .1)));
        v
    }

    /// Atomic flush: serialise → write to `<path>.tmp` → rename.
    /// Called by the background flush task.
    fn flush(&self) -> std::io::Result<()> {
        let snap = OnDiskFormat {
            entries: self
                .inner
                .read()
                .iter()
                .map(|((sid, host), entry)| OnDiskEntry {
                    stream_id: *sid,
                    host: host.clone(),
                    samples: entry.samples.clone(),
                })
                .collect(),
        };
        let body = serde_json::to_vec_pretty(&snap).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Background flush task. Watches `dirty`; on 5 s debounce, atomically writes
/// the store to disk. Cancel-safe (no shared mutable state outside the store
/// itself).
pub async fn run_flush_task(store: std::sync::Arc<MeasuredStore>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if !store.dirty.swap(false, Ordering::AcqRel) {
            continue;
        }
        match store.flush() {
            Ok(()) => debug!(path = %store.path.display(), "measured store flushed"),
            Err(e) => {
                warn!(path = %store.path.display(), err = %e, "measured store flush failed");
                // Re-set dirty so we retry next tick.
                store.dirty.store(true, Ordering::Release);
            }
        }
    }
}

// --- Per-play session accumulator ------------------------------------------

/// In-progress observation for a single (stream_id, host) play session.
/// Fields populated from two sources:
///   - classifier (`note_classification`): width/height/codec/pix_fmt/color_transfer/framerate
///   - segment hot-path (`note_segment_kbps`): EWMA of per-segment kbps
///
/// A background committer drains entries idle for ≥30 s, producing one
/// complete `Sample` per play session.
#[derive(Debug, Clone, Default)]
pub struct InProgress {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: Option<String>,
    pub pix_fmt: Option<String>,
    pub color_transfer: Option<String>,
    pub framerate: Option<f32>,
    pub bitrate_ewma_kbps: Option<f32>,
    pub dvb_unsafe: Option<bool>,
    /// Step 10 audio fields. Populated by `note_audio_classification` from
    /// the radio play path; remain `None` for TV plays.
    pub sample_rate_hz: Option<u32>,
    pub audio_channels: Option<u8>,
    /// Phase 0: slice-header walker result accumulated per play. Per-play
    /// samples currently never observe the slice walker (the per-segment
    /// hot path skips it — Phase 0 confines the walker to the sweep paths),
    /// but the field is plumbed so future phases can promote it if
    /// motivation arises. Stays `None` for now.
    pub h264_excess_refs: Option<bool>,
    pub last_activity: Option<Instant>,
}

impl InProgress {
    fn touch(&mut self) {
        self.last_activity = Some(Instant::now());
    }

    /// `Some(sample)` only when at least one classification arrived
    /// (otherwise we know nothing about the source's video shape and the
    /// sample is uninformative).
    fn into_sample(self) -> Option<Sample> {
        // Sample commits when EITHER the video classifier ran (width set)
        // OR the audio classifier ran (sample_rate_hz set). Radio plays
        // don't set width/height; without the second arm the audio sample
        // would be silently dropped at commit time.
        if self.width.is_none() && self.sample_rate_hz.is_none() {
            return None;
        }
        Some(Sample {
            at: time::OffsetDateTime::now_utc(),
            source: SampleSource::PerPlay,
            width: self.width.unwrap_or(0),
            height: self.height.unwrap_or(0),
            codec: self.codec,
            pix_fmt: self.pix_fmt,
            color_transfer: self.color_transfer,
            framerate: self.framerate,
            bitrate_kbps: self.bitrate_ewma_kbps.map(|v| v.round() as u32),
            dvb_unsafe: self.dvb_unsafe,
            sample_rate_hz: self.sample_rate_hz,
            audio_channels: self.audio_channels,
            h264_excess_refs: self.h264_excess_refs,
        })
    }
}

pub struct PerPlayAccumulator {
    inner: RwLock<HashMap<(u64, String), InProgress>>,
}

impl PerPlayAccumulator {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Called from `handle_ts_segment` after the TS classifier produces a
    /// `Classification`. Static fields are filled the first time; subsequent
    /// calls just refresh `last_activity`. Cheap — classifier itself caches
    /// per stream_id so this fires once per stream per play.
    #[allow(clippy::too_many_arguments)]
    pub fn note_classification(
        &self,
        stream_id: u64,
        host: &str,
        width: Option<u32>,
        height: Option<u32>,
        codec: Option<String>,
        pix_fmt: Option<String>,
        color_transfer: Option<String>,
        framerate: Option<f32>,
        dvb_unsafe: Option<bool>,
    ) {
        let mut g = self.inner.write();
        let ip = g.entry((stream_id, host.to_string())).or_default();
        if ip.width.is_none() {
            ip.width = width;
        }
        if ip.height.is_none() {
            ip.height = height;
        }
        if ip.codec.is_none() {
            ip.codec = codec;
        }
        if ip.pix_fmt.is_none() {
            ip.pix_fmt = pix_fmt;
        }
        if ip.color_transfer.is_none() {
            ip.color_transfer = color_transfer;
        }
        if ip.framerate.is_none() {
            ip.framerate = framerate;
        }
        if ip.dvb_unsafe.is_none() {
            ip.dvb_unsafe = dvb_unsafe;
        }
        ip.touch();
    }

    /// Called from the radio per-play path after an `AudioClassification`
    /// is extracted from an ADTS segment (Step 10). Static fields fill
    /// once; subsequent calls just refresh `last_activity`.
    pub fn note_audio_classification(
        &self,
        stream_id: u64,
        host: &str,
        sample_rate_hz: Option<u32>,
        audio_channels: Option<u8>,
    ) {
        let mut g = self.inner.write();
        let ip = g.entry((stream_id, host.to_string())).or_default();
        if ip.sample_rate_hz.is_none() {
            ip.sample_rate_hz = sample_rate_hz;
        }
        if ip.audio_channels.is_none() {
            ip.audio_channels = audio_channels;
        }
        ip.touch();
    }

    /// Called from `proxy_segment` per real (non-probe) TS segment with a
    /// known stream_id, host, and duration. Maintains a running EWMA of
    /// per-segment kbps.
    pub fn note_segment_kbps(&self, stream_id: u64, host: &str, kbps: f32) {
        const ALPHA: f32 = 0.3;
        let mut g = self.inner.write();
        let ip = g.entry((stream_id, host.to_string())).or_default();
        ip.bitrate_ewma_kbps = Some(match ip.bitrate_ewma_kbps {
            Some(prev) => ALPHA * kbps + (1.0 - ALPHA) * prev,
            None => kbps,
        });
        ip.touch();
    }

    /// Background committer task. Every 5 s, finds entries with no activity
    /// for ≥30 s and commits each as a single complete `Sample`.
    pub async fn run_committer(
        self: std::sync::Arc<Self>,
        store: std::sync::Arc<MeasuredStore>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        const QUIESCE: Duration = Duration::from_secs(30);
        loop {
            tick.tick().await;
            let now = Instant::now();
            let mut to_commit: Vec<((u64, String), InProgress)> = Vec::new();
            {
                let mut g = self.inner.write();
                g.retain(|key, ip| {
                    let idle = ip
                        .last_activity
                        .map(|t| now.duration_since(t))
                        .unwrap_or(Duration::ZERO);
                    if idle >= QUIESCE {
                        to_commit.push((key.clone(), ip.clone()));
                        false
                    } else {
                        true
                    }
                });
            }
            for ((sid, host), ip) in to_commit {
                if let Some(sample) = ip.into_sample() {
                    debug!(
                        stream_id = sid,
                        host = %host,
                        w = sample.width,
                        h = sample.height,
                        codec = ?sample.codec,
                        kbps = ?sample.bitrate_kbps,
                        "per-play sample committed"
                    );
                    store.push(sid, &host, sample);
                }
            }
        }
    }
}

impl Default for PerPlayAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(kbps: Option<u32>, w: u32, h: u32) -> Sample {
        Sample {
            at: time::OffsetDateTime::now_utc(),
            source: SampleSource::Sweep,
            width: w,
            height: h,
            codec: Some("h264".into()),
            pix_fmt: Some("yuv420p".into()),
            color_transfer: Some("bt709".into()),
            framerate: Some(50.0),
            bitrate_kbps: kbps,
            dvb_unsafe: None,
            sample_rate_hz: None,
            audio_channels: None,
            h264_excess_refs: None,
        }
    }

    #[test]
    fn push_evicts_oldest_at_window() {
        let mut e = MeasuredEntry::default();
        for k in 0..(WINDOW + 3) {
            e.push(sample(Some(k as u32 * 100), 1920, 1080));
        }
        assert_eq!(e.samples.len(), WINDOW);
        // The oldest 3 (0, 100, 200) were evicted; first remaining is 300.
        assert_eq!(e.samples.front().unwrap().bitrate_kbps, Some(300));
        assert_eq!(e.samples.back().unwrap().bitrate_kbps, Some(((WINDOW + 2) * 100) as u32));
    }

    #[test]
    fn aggregate_median_ignores_one_bad_sample() {
        let mut e = MeasuredEntry::default();
        // Four healthy samples around 4000 kbps + one placeholder-sized 50.
        for kbps in &[3950u32, 4020, 4080, 50, 4030] {
            e.push(sample(Some(*kbps), 1920, 1080));
        }
        let agg = e.aggregate().unwrap();
        // Median of [50, 3950, 4020, 4030, 4080] is 4020 — bad sample washed out.
        assert_eq!(agg.bitrate_kbps, Some(4020));
        // Most-recent for stable fields: last push had W/H 1920x1080.
        assert_eq!(agg.width, 1920);
        assert_eq!(agg.height, 1080);
    }

    #[test]
    fn aggregate_handles_all_none_bitrate() {
        let mut e = MeasuredEntry::default();
        for _ in 0..3 {
            e.push(sample(None, 1280, 720));
        }
        let agg = e.aggregate().unwrap();
        assert!(agg.bitrate_kbps.is_none());
        assert_eq!(agg.width, 1280);
        assert_eq!(agg.height, 720);
    }

    #[test]
    fn aggregate_returns_none_when_empty() {
        let e = MeasuredEntry::default();
        assert!(e.aggregate().is_none());
    }

    #[test]
    fn store_round_trip_via_disk() {
        let dir = tempdir_or_skip();
        let path = dir.join("measured_quality.json");
        let store = MeasuredStore::load_or_empty(path.clone());
        store.push(12345, "http://host.a", sample(Some(4500), 1920, 1080));
        store.push(12345, "http://host.a", sample(Some(4600), 1920, 1080));
        store.push(12345, "http://host.b", sample(Some(3000), 1280, 720));
        store.flush().expect("flush ok");

        let store2 = MeasuredStore::load_or_empty(path);
        assert!(store2.has_samples(12345, "http://host.a"));
        assert!(store2.has_samples(12345, "http://host.b"));
        let a = store2.get(12345, "http://host.a").unwrap();
        assert_eq!(a.width, 1920);
        // p50 of [4500, 4600] = 4600 (the upper middle by index).
        assert_eq!(a.bitrate_kbps, Some(4600));
    }

    #[test]
    fn store_missing_file_starts_empty_no_warn() {
        let dir = tempdir_or_skip();
        let path = dir.join("does-not-exist.json");
        let store = MeasuredStore::load_or_empty(path);
        assert!(!store.has_samples(1, "h"));
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn store_corrupt_file_starts_empty() {
        let dir = tempdir_or_skip();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"not valid json").unwrap();
        let store = MeasuredStore::load_or_empty(path);
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn in_progress_commit_requires_classification() {
        // Bitrate alone (no W/H from classifier) shouldn't commit — we don't
        // know enough about the source to record a useful sample.
        let ip = InProgress {
            bitrate_ewma_kbps: Some(4000.0),
            last_activity: Some(Instant::now()),
            ..Default::default()
        };
        assert!(ip.into_sample().is_none());
    }

    #[test]
    fn in_progress_commit_with_classification_yields_complete_sample() {
        let ip = InProgress {
            width: Some(1920),
            height: Some(1080),
            codec: Some("h264".into()),
            pix_fmt: Some("yuv420p".into()),
            color_transfer: Some("bt709".into()),
            framerate: Some(50.0),
            bitrate_ewma_kbps: Some(4523.4),
            dvb_unsafe: Some(false),
            sample_rate_hz: None,
            audio_channels: None,
            h264_excess_refs: None,
            last_activity: Some(Instant::now()),
        };
        let s = ip.into_sample().unwrap();
        assert_eq!(s.width, 1920);
        assert_eq!(s.height, 1080);
        assert_eq!(s.bitrate_kbps, Some(4523));
        assert!(matches!(s.source, SampleSource::PerPlay));
    }

    #[test]
    fn per_play_accumulator_ewma_smooths_bitrate() {
        let acc = PerPlayAccumulator::new();
        acc.note_classification(
            10,
            "http://h",
            Some(1920),
            Some(1080),
            Some("h264".into()),
            None,
            None,
            None,
            None,
        );
        // Three segments at varying rates — EWMA should land somewhere in the
        // middle, weighted toward the most recent.
        acc.note_segment_kbps(10, "http://h", 4000.0);
        acc.note_segment_kbps(10, "http://h", 5000.0);
        acc.note_segment_kbps(10, "http://h", 6000.0);

        let g = acc.inner.read();
        let ip = g.get(&(10, "http://h".to_string())).unwrap();
        // alpha=0.3: first sets to 4000; then 0.3*5000+0.7*4000=4300; then
        // 0.3*6000+0.7*4300=4810.
        let v = ip.bitrate_ewma_kbps.unwrap();
        assert!((v - 4810.0).abs() < 1.0, "got {v}");
    }

    fn sample_excess(codec: &str, h264_excess: Option<bool>) -> Sample {
        Sample {
            at: time::OffsetDateTime::now_utc(),
            source: SampleSource::Sweep,
            width: 1920,
            height: 1080,
            codec: Some(codec.into()),
            pix_fmt: Some("yuv420p".into()),
            color_transfer: None,
            framerate: None,
            bitrate_kbps: Some(4000),
            dvb_unsafe: Some(false),
            sample_rate_hz: None,
            audio_channels: None,
            h264_excess_refs: h264_excess,
        }
    }

    #[test]
    fn stability_gate_on_for_one_positive() {
        let mut e = MeasuredEntry::default();
        e.push(sample_excess("h264", Some(true)));
        let agg = e.aggregate().unwrap();
        assert_eq!(agg.h264_excess_refs_state, ExcessRefsState::On);
        assert!(agg.caps_required.iter().any(|c| c == "h264_excess_refs"));
    }

    #[test]
    fn stability_gate_off_after_two_negatives() {
        let mut e = MeasuredEntry::default();
        e.push(sample_excess("h264", Some(false)));
        e.push(sample_excess("h264", Some(false)));
        let agg = e.aggregate().unwrap();
        assert_eq!(agg.h264_excess_refs_state, ExcessRefsState::Off);
        assert!(!agg.caps_required.iter().any(|c| c == "h264_excess_refs"));
    }

    #[test]
    fn stability_gate_unknown_with_one_negative_is_fail_closed() {
        // R2 finding: UNKNOWN should keep the cap (plan §5 "UNKNOWN
        // handling is fail-closed"). A single-negative window must still
        // emit `h264_excess_refs` so strict clients don't get the variant
        // until a second corroborating negative arrives.
        let mut e = MeasuredEntry::default();
        e.push(sample_excess("h264", Some(false)));
        let agg = e.aggregate().unwrap();
        assert_eq!(agg.h264_excess_refs_state, ExcessRefsState::Unknown);
        assert!(
            agg.caps_required.iter().any(|c| c == "h264_excess_refs"),
            "UNKNOWN must fail closed (keep cap); caps_required={:?}",
            agg.caps_required,
        );
    }

    #[test]
    fn stability_gate_unknown_does_not_add_cap_for_hevc() {
        // UNKNOWN for HEVC = vacuously decisive (predicate doesn't apply),
        // so the cap should NOT be emitted even when state is Unknown.
        let s = sample_excess("hevc", None);
        let caps = derive_variant_caps(&s, ExcessRefsState::Unknown);
        assert!(
            !caps.iter().any(|c| c == "h264_excess_refs"),
            "non-H.264 codec must never carry h264_excess_refs cap; caps={caps:?}"
        );
    }

    #[test]
    fn stability_gate_na_for_hevc() {
        let mut e = MeasuredEntry::default();
        e.push(sample_excess("hevc", None));
        let agg = e.aggregate().unwrap();
        assert_eq!(agg.h264_excess_refs_state, ExcessRefsState::NotApplicable);
        assert!(!agg.caps_required.iter().any(|c| c == "h264_excess_refs"));
    }

    #[test]
    fn derive_variant_caps_tv_baseline_preserves_aac() {
        let s = sample_excess("h264", None);
        let caps = derive_variant_caps(&s, ExcessRefsState::Off);
        assert!(caps.iter().any(|c| c == "aac"));
        assert!(caps.iter().any(|c| c == "live_video_hls"));
        assert!(caps.iter().any(|c| c == "h264"));
        assert!(caps.iter().any(|c| c == "hls"));
    }

    #[test]
    fn derive_variant_caps_radio_baseline() {
        let s = Sample {
            at: time::OffsetDateTime::now_utc(),
            source: SampleSource::Sweep,
            width: 0,
            height: 0,
            codec: Some("aac".into()),
            pix_fmt: None,
            color_transfer: None,
            framerate: None,
            bitrate_kbps: Some(128),
            dvb_unsafe: None,
            sample_rate_hz: Some(48000),
            audio_channels: Some(2),
            h264_excess_refs: None,
        };
        let caps = derive_variant_caps(&s, ExcessRefsState::NotApplicable);
        assert!(caps.iter().any(|c| c == "hls"));
        assert!(caps.iter().any(|c| c == "live_audio_only_hls"));
        assert!(!caps.iter().any(|c| c == "live_video_hls"));
        assert!(!caps.iter().any(|c| c == "aac"));
    }

    #[test]
    fn derive_variant_caps_hevc_main10() {
        let mut s = sample_excess("hevc", None);
        s.pix_fmt = Some("yuv420p10le".into());
        let caps = derive_variant_caps(&s, ExcessRefsState::NotApplicable);
        assert!(caps.iter().any(|c| c == "hevc"));
        assert!(caps.iter().any(|c| c == "hevc_main10"));
    }

    #[test]
    fn derive_variant_caps_dvb_safe() {
        let mut s = sample_excess("h264", None);
        s.dvb_unsafe = Some(true);
        let caps = derive_variant_caps(&s, ExcessRefsState::Off);
        assert!(caps.iter().any(|c| c == "dvb_safe"));
    }

    /// Get a unique temp dir, or skip the test if /tmp isn't writable.
    /// Avoids pulling tempfile as a dev-dep.
    fn tempdir_or_skip() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "iptv-proxy-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
}
