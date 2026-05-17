// State-machine cool-off with disk persistence.
//
// Replaces the original binary blacklist (a windowed fail counter + a separate
// demote bucket) with a single per-URL state machine: each failure bumps a
// cool-off step (0..=4 → none / 1 min / 5 min / 30 min / 6 h); clean-play
// heartbeats reset it. The step is consumed as a rank-tuple penalty in Step 4;
// for now it's also surfaced via the existing diagnostic / admin getters so
// callers in proxy.rs and api.rs keep compiling unchanged.
//
// The wall-clock fields use `OffsetDateTime` (not `Instant`) so the store
// round-trips through `data/blacklist.json` cleanly — see `OnDiskFormat` and
// `run_flush_task` below.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::{debug, warn};

use crate::config::BlacklistConfig;

/// Cool-off step ceiling (0 = no cool-off, 4 = the longest step).
pub const MAX_COOL_OFF_STEP: u8 = 4;

/// Diagnostic threshold for `is_url_failed`. Matches the spirit of the old
/// `url_fail_threshold = 3` default — three fails escalate to step 3.
const FAILED_DIAG_STEP: u8 = 3;

/// Failure semantics. See plan §Approach Step 2 for the per-variant matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Upstream 5xx / connection timeout / placeholder manifest. Bumps
    /// cool-off AND blames the host (so `is_host_bad` can pick up genuinely
    /// dead upstreams).
    ServerSide,
    /// Mid-playback decoder error — the TV got `canplay` but then choked.
    /// Bumps cool-off but does NOT blame the host: a decoder mismatch says
    /// nothing about whether the upstream is up.
    ClientPostCanplay,
    /// Slow-to-start, watchdog, manifest fetch error. Per architecture.md
    /// §4 this is not an instability signal — log-only no-op for cool-off.
    ClientPreCanplay,
}

/// Per-URL cool-off state. `consecutive_fails` tracks the un-reset failure
/// count for diagnostics; `cool_off_step` is the rank-tuple penalty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UrlState {
    #[serde(default)]
    pub cool_off_step: u8,
    #[serde(default)]
    pub consecutive_fails: u8,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_error_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_heartbeat_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostFailEntry {
    pub host: String,
    pub distinct_streams_failed: usize,
    pub last_fail_secs_ago: u64,
}

#[derive(Debug, Default)]
struct Inner {
    per_url: HashMap<String, UrlState>,
    host_streams: HashMap<String, HashMap<String, OffsetDateTime>>,
    last_known_good: HashMap<String, (String, OffsetDateTime)>,
}

/// Flat on-disk shape. Mirrors `MeasuredStore::OnDiskFormat` — lists instead
/// of maps so JSON round-trips without tuple-keyed map quirks.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDiskFormat {
    #[serde(default)]
    per_url: Vec<OnDiskUrl>,
    #[serde(default)]
    host_streams: Vec<OnDiskHost>,
    #[serde(default)]
    last_known_good: Vec<OnDiskLkg>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OnDiskUrl {
    url: String,
    #[serde(flatten)]
    state: UrlState,
}

#[derive(Debug, Serialize, Deserialize)]
struct OnDiskHost {
    host: String,
    #[serde(default)]
    streams: Vec<OnDiskStream>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OnDiskStream {
    stream_id: String,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
}

#[derive(Debug, Serialize, Deserialize)]
struct OnDiskLkg {
    channel: String,
    url: String,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
}

pub struct Blacklist {
    inner: RwLock<Inner>,
    config: BlacklistConfig,
    /// `None` = in-memory only (tests). `Some(path)` = backed by disk; the
    /// flush task atomically rewrites this file on a 5 s debounce.
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

impl Blacklist {
    /// In-memory-only constructor. Production goes through `load_or_empty`;
    /// this exists only for unit tests in this file. Gating on `#[cfg(test)]`
    /// keeps the binary's call graph honest about who needs disk persistence.
    #[cfg(test)]
    pub fn new(config: BlacklistConfig) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            config,
            path: None,
            dirty: AtomicBool::new(false),
        }
    }

    /// Load from disk; returns empty store if the file is missing or
    /// corrupted (warned, not fatal — cool-off rebuilds from new failures).
    pub fn load_or_empty(config: BlacklistConfig, path: PathBuf) -> Self {
        let inner = match std::fs::read_to_string(&path) {
            Ok(body) => match serde_json::from_str::<OnDiskFormat>(&body) {
                Ok(f) => Inner::from_ondisk(f),
                Err(e) => {
                    warn!(path = %path.display(), err = %e, "blacklist: parse failed, starting empty");
                    Inner::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Inner::default(),
            Err(e) => {
                warn!(path = %path.display(), err = %e, "blacklist: read failed, starting empty");
                Inner::default()
            }
        };
        Self {
            inner: RwLock::new(inner),
            config,
            path: Some(path),
            dirty: AtomicBool::new(false),
        }
    }

    /// Record a failure of the given kind.
    pub fn note_failure(&self, url: &str, kind: FailureKind) {
        // ClientPreCanplay is a complete no-op for cool-off state. The
        // variant exists so the feedback API can distinguish a slow-to-start
        // blip from a real failure when logging; per architecture.md §4
        // slow-to-start is not an instability signal.
        if kind == FailureKind::ClientPreCanplay {
            return;
        }
        let now = OffsetDateTime::now_utc();
        let mut g = self.inner.write();
        let state = g.per_url.entry(url.to_string()).or_default();
        state.consecutive_fails = state.consecutive_fails.saturating_add(1);
        state.cool_off_step = (state.cool_off_step + 1).min(MAX_COOL_OFF_STEP);
        state.last_error_at = Some(now);
        // Only ServerSide blames the host. A TV-decoder mishap doesn't say
        // anything about whether the upstream is up — host-bookkeeping there
        // would have the host marked bad on perfectly-healthy upstreams.
        if kind == FailureKind::ServerSide {
            if let (Some(host), Some(sid)) = (host_of(url), stream_id_of(url)) {
                g.host_streams.entry(host).or_default().insert(sid, now);
            }
        }
        drop(g);
        self.dirty.store(true, Ordering::Release);
    }

    /// Record a clean-play heartbeat. Resets cool-off when:
    ///   1. `now - last_error_at >= clean_play_reset_secs`, AND
    ///   2. the *previous* heartbeat was within `heartbeat_window_secs`
    ///      (proves sustained playback — a single late heartbeat after a
    ///      long pause is not enough).
    ///
    /// No-op when the URL has no recorded state (never failed) and no-op
    /// for cool-off when `last_error_at` is `None`.
    pub fn note_heartbeat(&self, url: &str) {
        let now = OffsetDateTime::now_utc();
        let mut g = self.inner.write();
        let Some(state) = g.per_url.get_mut(url) else {
            // URL has never been recorded — don't create a bookkeeping entry
            // just to record a heartbeat. The entry only matters once there
            // is a failure to reset against.
            return;
        };
        let prev_heartbeat = state.last_heartbeat_at;
        state.last_heartbeat_at = Some(now);
        let Some(err_at) = state.last_error_at else {
            drop(g);
            self.dirty.store(true, Ordering::Release);
            return;
        };
        let since_err = (now - err_at).whole_seconds().max(0) as u64;
        // Previous heartbeat must be fresh enough that we can vouch for
        // continuous playback. First heartbeat after error has no
        // predecessor → cannot reset on this single beat alone.
        let prev_fresh = prev_heartbeat
            .map(|t| (now - t).whole_seconds().max(0) as u64 <= self.config.heartbeat_window_secs)
            .unwrap_or(false);
        if since_err >= self.config.clean_play_reset_secs && prev_fresh {
            state.cool_off_step = 0;
            state.consecutive_fails = 0;
            state.last_error_at = None;
        }
        drop(g);
        self.dirty.store(true, Ordering::Release);
    }

    /// Returns the URL's cool-off step (0..=4) — consumed by the rank tuple
    /// in Step 4 (`source_rank_key_tv` / `source_rank_key_radio`).
    /// Returns 0 for never-failed URLs.
    pub fn cool_off_penalty(&self, url: &str) -> i32 {
        self.cool_off_step_of(url) as i32
    }

    fn cool_off_step_of(&self, url: &str) -> u8 {
        self.inner
            .read()
            .per_url
            .get(url)
            .map(|s| s.cool_off_step)
            .unwrap_or(0)
    }

    pub fn note_url_succeeded(&self, channel_key: &str, url: &str) {
        let now = OffsetDateTime::now_utc();
        let mut g = self.inner.write();
        g.last_known_good
            .insert(channel_key.to_string(), (url.to_string(), now));
        drop(g);
        self.dirty.store(true, Ordering::Release);
    }

    /// Diagnostic / archive paths: returns the channel's recorded LKG URL
    /// if one exists and is < 24 h old. Live-play candidate selection no
    /// longer reads this — it goes through `last_known_good_age` and the
    /// `lkg_bonus` slot in the rank tuple instead. Kept as a public
    /// surface so a future diagnostic endpoint (or Phase 7 cap-matrix
    /// debugging) can read it without re-deriving from `per_url`.
    #[allow(dead_code)]
    pub fn last_known_good(&self, channel_key: &str) -> Option<String> {
        let g = self.inner.read();
        let (url, at) = g.last_known_good.get(channel_key)?;
        let elapsed = (OffsetDateTime::now_utc() - *at).whole_seconds().max(0) as u64;
        if elapsed > 86400 {
            return None;
        }
        Some(url.clone())
    }

    /// Returns the age of the channel's LKG only if it equals `url`. Used
    /// by the rank-tuple `lkg_bonus` (Step 5): we only want to award the
    /// bonus to the URL that *is* the LKG, not to other candidates for
    /// the same channel. `None` if no LKG, or LKG points elsewhere, or
    /// LKG older than 24 h (matches `last_known_good`'s freshness gate).
    pub fn last_known_good_age(&self, channel_key: &str, url: &str) -> Option<Duration> {
        let g = self.inner.read();
        let (lkg_url, at) = g.last_known_good.get(channel_key)?;
        if lkg_url != url {
            return None;
        }
        let elapsed_secs = (OffsetDateTime::now_utc() - *at).whole_seconds().max(0) as u64;
        if elapsed_secs > 86400 {
            return None;
        }
        Some(Duration::from_secs(elapsed_secs))
    }

    /// Diagnostic getter. Returns true once cool-off has crossed the step
    /// that mapped to "blacklisted" under the old threshold defaults
    /// (three consecutive fails). NO LONGER USED for live-play candidate
    /// selection (Step 4 dropped that); still consulted by the EPG, probe-
    /// redirect, and catchup paths (plan §4 line 134) — those have
    /// different reliability requirements.
    pub fn is_url_failed(&self, url: &str) -> bool {
        self.cool_off_step_of(url) >= FAILED_DIAG_STEP
    }

    pub fn is_host_bad(&self, host: &str) -> bool {
        let g = self.inner.read();
        let Some(streams) = g.host_streams.get(host) else { return false };
        let ttl_secs = self.config.host_ttl_secs;
        let now = OffsetDateTime::now_utc();
        let recent = streams
            .values()
            .filter(|t| ((now - **t).whole_seconds().max(0) as u64) < ttl_secs)
            .count();
        recent >= self.config.host_fail_threshold
    }

    /// Count of URLs with any cool-off state (step > 0 or just heartbeat-
    /// tracked). Feeds `BlacklistStatusDto.url_states_count` — the single
    /// number that replaces the old `failed_urls + demoted_urls` split now
    /// that cool-off is a unified step.
    pub fn per_url_count(&self) -> usize {
        self.inner.read().per_url.len()
    }

    pub fn snapshot_hosts(&self) -> Vec<HostFailEntry> {
        let g = self.inner.read();
        let now = OffsetDateTime::now_utc();
        let ttl_secs = self.config.host_ttl_secs;
        g.host_streams
            .iter()
            .map(|(h, streams)| {
                let recent: Vec<OffsetDateTime> = streams
                    .values()
                    .filter(|t| ((now - **t).whole_seconds().max(0) as u64) < ttl_secs)
                    .copied()
                    .collect();
                let last = recent.iter().max().copied().unwrap_or(now);
                HostFailEntry {
                    host: h.clone(),
                    distinct_streams_failed: recent.len(),
                    last_fail_secs_ago: (now - last).whole_seconds().max(0) as u64,
                }
            })
            .filter(|e| e.distinct_streams_failed > 0)
            .collect()
    }

    /// Atomic flush: serialise → write to `<path>.tmp` → rename. No-op for
    /// in-memory stores (tests).
    fn flush(&self) -> std::io::Result<()> {
        let Some(path) = self.path.as_ref() else { return Ok(()); };
        let snap = self.inner.read().to_ondisk();
        let body = serde_json::to_vec_pretty(&snap)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

impl Inner {
    fn to_ondisk(&self) -> OnDiskFormat {
        OnDiskFormat {
            per_url: self
                .per_url
                .iter()
                .map(|(u, s)| OnDiskUrl { url: u.clone(), state: s.clone() })
                .collect(),
            host_streams: self
                .host_streams
                .iter()
                .map(|(h, streams)| OnDiskHost {
                    host: h.clone(),
                    streams: streams
                        .iter()
                        .map(|(sid, at)| OnDiskStream { stream_id: sid.clone(), at: *at })
                        .collect(),
                })
                .collect(),
            last_known_good: self
                .last_known_good
                .iter()
                .map(|(c, (u, at))| OnDiskLkg {
                    channel: c.clone(),
                    url: u.clone(),
                    at: *at,
                })
                .collect(),
        }
    }

    fn from_ondisk(f: OnDiskFormat) -> Self {
        let per_url = f.per_url.into_iter().map(|e| (e.url, e.state)).collect();
        let host_streams = f
            .host_streams
            .into_iter()
            .map(|h| {
                (
                    h.host,
                    h.streams.into_iter().map(|s| (s.stream_id, s.at)).collect(),
                )
            })
            .collect();
        let last_known_good = f
            .last_known_good
            .into_iter()
            .map(|l| (l.channel, (l.url, l.at)))
            .collect();
        Self { per_url, host_streams, last_known_good }
    }
}

/// Background flush task. Mirror of `measured::run_flush_task` — watches
/// `dirty`; on 5 s debounce, atomically writes the store to disk.
pub async fn run_flush_task(store: std::sync::Arc<Blacklist>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if !store.dirty.swap(false, Ordering::AcqRel) {
            continue;
        }
        let path_disp = store
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        match store.flush() {
            Ok(()) => debug!(path = %path_disp, "blacklist flushed"),
            Err(e) => {
                warn!(path = %path_disp, err = %e, "blacklist flush failed");
                store.dirty.store(true, Ordering::Release);
            }
        }
    }
}

fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let host = after_scheme.split('/').next()?;
    Some(format!("{}://{}", url.split_once("://")?.0, host))
}

fn stream_id_of(url: &str) -> Option<String> {
    let after_live = url.split("/live/").nth(1)?;
    let parts: Vec<&str> = after_live.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let last = parts[2];
    let id = last.split('.').next()?;
    Some(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BlacklistConfig;

    fn cfg() -> BlacklistConfig {
        BlacklistConfig {
            host_fail_threshold: 4,
            host_ttl_secs: 3600,
            cool_off_steps_secs: [60, 300, 1800, 21600],
            heartbeat_window_secs: 60,
            clean_play_reset_secs: 300,
        }
    }

    /// Test helper: inject a UrlState directly so we can exercise heartbeat
    /// reset without sleeping. Avoids time-faking machinery for the only
    /// case that needs synthetic timestamps.
    fn set_state(bl: &Blacklist, url: &str, s: UrlState) {
        bl.inner.write().per_url.insert(url.to_string(), s);
    }

    #[test]
    fn parses_url_parts() {
        let u = "http://cf.8kgaminghub.shop/live/USER/PASS/12345.m3u8";
        assert_eq!(host_of(u).unwrap(), "http://cf.8kgaminghub.shop");
        assert_eq!(stream_id_of(u).unwrap(), "12345");
    }

    #[test]
    fn server_side_failures_escalate_through_steps() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        for step in 1..=4 {
            bl.note_failure(u, FailureKind::ServerSide);
            assert_eq!(bl.cool_off_penalty(u), step, "after {step} fail(s)");
        }
        // Capped at MAX_COOL_OFF_STEP.
        bl.note_failure(u, FailureKind::ServerSide);
        bl.note_failure(u, FailureKind::ServerSide);
        assert_eq!(bl.cool_off_penalty(u), MAX_COOL_OFF_STEP as i32);
    }

    #[test]
    fn client_post_canplay_escalates_like_server_side() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        for step in 1..=4 {
            bl.note_failure(u, FailureKind::ClientPostCanplay);
            assert_eq!(bl.cool_off_penalty(u), step);
        }
    }

    #[test]
    fn client_pre_canplay_is_a_noop() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        for _ in 0..10 {
            bl.note_failure(u, FailureKind::ClientPreCanplay);
        }
        assert_eq!(bl.cool_off_penalty(u), 0);
        // Also nothing in per_url at all — the variant doesn't even create
        // a bookkeeping entry.
        assert!(bl.inner.read().per_url.is_empty());
        // And host_streams stays empty.
        assert!(bl.inner.read().host_streams.is_empty());
    }

    #[test]
    fn host_blame_only_on_server_side() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_failure(u, FailureKind::ClientPostCanplay);
        bl.note_failure(u, FailureKind::ClientPostCanplay);
        assert!(
            bl.inner.read().host_streams.is_empty(),
            "post-canplay must not blame the host"
        );
        bl.note_failure(u, FailureKind::ServerSide);
        assert!(
            !bl.inner.read().host_streams.is_empty(),
            "ServerSide blames the host"
        );
    }

    #[test]
    fn heartbeat_resets_after_clean_window_with_fresh_predecessor() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        let now = OffsetDateTime::now_utc();
        let cfg_ref = &bl.config;
        // Synthesise: error 6 min ago, previous heartbeat 30 s ago.
        // Cool-off step is at 2 — should reset cleanly.
        set_state(
            &bl,
            u,
            UrlState {
                cool_off_step: 2,
                consecutive_fails: 2,
                last_error_at: Some(
                    now - Duration::from_secs(cfg_ref.clean_play_reset_secs + 60),
                ),
                last_heartbeat_at: Some(now - Duration::from_secs(30)),
            },
        );
        bl.note_heartbeat(u);
        let g = bl.inner.read();
        let s = g.per_url.get(u).unwrap();
        assert_eq!(s.cool_off_step, 0);
        assert_eq!(s.consecutive_fails, 0);
        assert!(s.last_error_at.is_none());
    }

    #[test]
    fn heartbeat_does_not_reset_without_fresh_predecessor() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        let now = OffsetDateTime::now_utc();
        // Error 6 min ago, but no prior heartbeat — this is the first
        // heartbeat after the error. Spec: don't reset on a single
        // post-pause heartbeat; require sustained playback.
        set_state(
            &bl,
            u,
            UrlState {
                cool_off_step: 2,
                consecutive_fails: 2,
                last_error_at: Some(now - Duration::from_secs(400)),
                last_heartbeat_at: None,
            },
        );
        bl.note_heartbeat(u);
        let g = bl.inner.read();
        let s = g.per_url.get(u).unwrap();
        assert_eq!(s.cool_off_step, 2, "first beat after error doesn't reset");
        assert!(s.last_heartbeat_at.is_some());
    }

    #[test]
    fn heartbeat_does_not_reset_inside_clean_play_window() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        let now = OffsetDateTime::now_utc();
        // Error 60 s ago (well inside the 300 s clean-play window).
        set_state(
            &bl,
            u,
            UrlState {
                cool_off_step: 1,
                consecutive_fails: 1,
                last_error_at: Some(now - Duration::from_secs(60)),
                last_heartbeat_at: Some(now - Duration::from_secs(30)),
            },
        );
        bl.note_heartbeat(u);
        let g = bl.inner.read();
        let s = g.per_url.get(u).unwrap();
        assert_eq!(s.cool_off_step, 1, "too soon after error to reset");
    }

    #[test]
    fn heartbeat_is_noop_when_no_prior_state() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_heartbeat(u);
        // No entry should have been created — we don't bookkeep heartbeats
        // for never-failed URLs.
        assert!(bl.inner.read().per_url.is_empty());
    }

    #[test]
    fn heartbeat_is_cool_off_noop_when_last_error_is_none() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        // Manually seed a state with no error recorded.
        set_state(
            &bl,
            u,
            UrlState {
                cool_off_step: 0,
                consecutive_fails: 0,
                last_error_at: None,
                last_heartbeat_at: Some(OffsetDateTime::now_utc() - Duration::from_secs(30)),
            },
        );
        bl.note_heartbeat(u);
        let g = bl.inner.read();
        let s = g.per_url.get(u).unwrap();
        assert_eq!(s.cool_off_step, 0);
        assert!(s.last_heartbeat_at.is_some());
    }

    #[test]
    fn note_url_succeeded_tracks_lkg_without_resetting_cool_off() {
        let bl = Blacklist::new(cfg());
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_failure(u, FailureKind::ServerSide);
        bl.note_failure(u, FailureKind::ServerSide);
        assert_eq!(bl.cool_off_penalty(u), 2);
        bl.note_url_succeeded("chan", u);
        assert_eq!(bl.last_known_good("chan").as_deref(), Some(u));
        // Successful playlist fetch is not the heartbeat path — it doesn't
        // reset cool-off on its own. The clean-play heartbeat does that.
        assert_eq!(bl.cool_off_penalty(u), 2);
    }

    #[test]
    fn last_known_good_age_only_matches_recorded_url() {
        let bl = Blacklist::new(cfg());
        bl.note_url_succeeded("rtp1", "http://a");
        // Same URL → Some(small age).
        let age = bl.last_known_good_age("rtp1", "http://a").expect("matches");
        assert!(age < Duration::from_secs(2));
        // Different URL → None (don't award the bonus to a non-LKG sibling).
        assert!(bl.last_known_good_age("rtp1", "http://b").is_none());
        // Unknown channel → None.
        assert!(bl.last_known_good_age("missing", "http://a").is_none());
    }

    #[test]
    fn persistence_round_trip_via_disk() {
        let dir = tempdir_or_skip();
        let path = dir.join("blacklist.json");
        {
            let bl = Blacklist::load_or_empty(cfg(), path.clone());
            bl.note_failure("http://h/live/U/P/1.m3u8", FailureKind::ServerSide);
            bl.note_failure("http://h/live/U/P/1.m3u8", FailureKind::ServerSide);
            bl.note_failure("http://h/live/U/P/2.m3u8", FailureKind::ClientPostCanplay);
            bl.note_url_succeeded("chan", "http://h/live/U/P/3.m3u8");
            bl.flush().expect("flush ok");
        }
        let bl2 = Blacklist::load_or_empty(cfg(), path);
        assert_eq!(bl2.cool_off_penalty("http://h/live/U/P/1.m3u8"), 2);
        assert_eq!(bl2.cool_off_penalty("http://h/live/U/P/2.m3u8"), 1);
        assert_eq!(
            bl2.last_known_good("chan").as_deref(),
            Some("http://h/live/U/P/3.m3u8")
        );
        // host_streams persisted too (ServerSide blamed the host).
        assert!(!bl2.inner.read().host_streams.is_empty());
    }

    #[test]
    fn load_missing_file_starts_empty() {
        let dir = tempdir_or_skip();
        let path = dir.join("does-not-exist.json");
        let bl = Blacklist::load_or_empty(cfg(), path);
        assert_eq!(bl.per_url_count(), 0);
        assert!(bl.snapshot_hosts().is_empty());
    }

    #[test]
    fn load_corrupt_file_starts_empty() {
        let dir = tempdir_or_skip();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"not valid json").unwrap();
        let bl = Blacklist::load_or_empty(cfg(), path);
        assert_eq!(bl.per_url_count(), 0);
    }

    /// Get a unique temp dir, mirroring measured.rs::tests::tempdir_or_skip.
    /// Avoids pulling tempfile as a dev-dep.
    fn tempdir_or_skip() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "iptv-proxy-blacklist-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
}
