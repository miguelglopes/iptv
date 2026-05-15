use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};

use crate::api::request_base_url;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::{debug, info, warn};
use url::Url;

use crate::canonical::CanonicalChannel;
#[cfg(test)]
use crate::canonical::CanonicalSource;
use crate::codec::{classify_ts_chunk, strip_subtitle_pids};
use crate::measured::{MeasuredQuality, MeasuredStore};
use crate::play_log::{AttemptOutcome, PlayAttempt, PlayEvent, PlayLog};
use crate::probe::is_placeholder_manifest;
use crate::state::AppState;

const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

/// Flip to `false` if pre-flight check 3 (see docs/plan-measured-quality.md)
/// reveals the B4 chipset can't decode HEVC main10. Inline constant rather
/// than config plumbing — same operational cost (change + redeploy) and
/// one less moving part. Setting to false makes the rank key treat 10-bit
/// HEVC as 8-bit SDR (drops HDR rank), demoting it relative to working H.264.
const TV_DECODES_HEVC_MAIN10: bool = true;

/// RAII counter for in-flight `/play/*` requests. The bootstrap sweep checks
/// this and yields when non-zero so it doesn't compete with users for the
/// provider's connection slots. Owns an Arc clone so it can outlive any
/// particular borrow of AppState.
struct ActivePlayGuard {
    counter: Arc<AtomicUsize>,
}

impl ActivePlayGuard {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter: Arc::clone(counter) }
    }
}

impl Drop for ActivePlayGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    url: String,
    host: String,
    /// Upstream stream_id. For Xtream sources, parsed from the URL pattern.
    /// For radio (`direct_source`), copied from `CanonicalSource.stream_id`
    /// (a synthetic high-bit-set value); radio URLs don't carry it in their
    /// path, so propagation through `Candidate` is the only way the
    /// measurement layer can key by `(stream_id, host)`.
    stream_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentToken {
    u: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    p: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    c: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    probe: bool,
    /// Segment duration in seconds, parsed from the upstream's `#EXTINF:`
    /// tag at rewrite time. Used by `proxy_segment` to compute per-segment
    /// kbps for the per-play bitrate EWMA. Absent on init segments and on
    /// any path that doesn't go through `rewrite_playlist` (e.g. test mints).
    #[serde(default, rename = "d", skip_serializing_if = "Option::is_none")]
    d: Option<f32>,
    /// Upstream host that served the playlist. Used to key per-(stream_id,
    /// host) measurement samples. Same lifecycle as `d`: set by
    /// `rewrite_playlist`, absent on older tokens.
    #[serde(default, rename = "h", skip_serializing_if = "Option::is_none")]
    h: Option<String>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn de_bool_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Bool(b) => Ok(b),
        serde_json::Value::Number(n) => Ok(n.as_u64().map(|x| x != 0).unwrap_or(false)),
        serde_json::Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "" | "0" | "false" | "no" | "off" => Ok(false),
            other => Err(D::Error::custom(format!("not a bool: {other}"))),
        },
        _ => Err(D::Error::custom("expected bool")),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PlayParams {
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
    /// Capability-probe request marker. Accepts `1`/`0`, `true`/`false`,
    /// `yes`/`no`. Axum's Query extractor errors on plain `?probe=1` against
    /// a bare `bool` field — the lenient parser tolerates both shapes.
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub probe: bool,
    /// Client-generated play-id. Threaded into the play URL as `?pid=<hex>`
    /// so the server can attribute this exact attempt back to a specific
    /// upstream choice when the client later reports a failure (avoids the
    /// LKG race when concurrent clients play the same channel). Optional —
    /// when absent the server falls back to its own counter for logging and
    /// to LKG-based blame for feedback (legacy behaviour).
    #[serde(default)]
    pub pid: Option<String>,
}

pub async fn play_playlist(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(params): Query<PlayParams>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let key = name.trim_end_matches(".m3u8").trim_end_matches(".ts");
    let probe_request = params.probe;
    // Track active real plays so the bootstrap sweep can yield to user
    // requests for the provider's connection slots. The probe-mode requests
    // (capability probe) don't take this guard — they're cheap and we don't
    // want them gating sweep starts. The guard drops at function exit
    // regardless of which path returns the response.
    let _active_play_guard = if probe_request {
        None
    } else {
        Some(ActivePlayGuard::new(&state.active_plays))
    };
    let catchup_request = parse_catchup_params(&params)?;
    let public_base = request_base_url(&headers, state.config.public_base_url.as_deref());
    // Use the client-supplied pid when present so feedback can blame the same
    // upstream the client actually saw. Sanitize defensively — pid is opaque to
    // the proxy but ends up in tracing fields and log files. Truncate to keep
    // memory bounded under a hostile client.
    let client_pid = params
        .pid
        .as_deref()
        .map(sanitize_pid)
        .filter(|s| !s.is_empty());
    let play_id = if probe_request {
        "probe".to_string()
    } else {
        client_pid
            .clone()
            .unwrap_or_else(|| state.play_log.next_id())
    };

    let snap = state.catalog.snapshot();
    let channel = snap
        .lookup(key)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, format!("unknown channel: {key}")))?;

    if let Some(req) = catchup_request {
        return catchup_play(state, channel, req, &public_base, &play_id, client_pid.as_deref()).await;
    }

    let candidates = build_candidates(&state, &channel);
    if candidates.is_empty() {
        warn!(play = %play_id, channel = %channel.key, "no candidate sources for channel");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no candidate sources for channel".into()));
    }

    let per_attempt = Duration::from_secs(state.config.proxy.per_attempt_timeout_secs);
    // Probe requests are run from the boot-time capability detector — the
    // client gives up after a few seconds. Cap the total budget so we fail
    // fast on a broken probe channel instead of dragging through the full
    // failover pipeline (which would timeout client-side anyway and falsely
    // report "no live_video_hls cap"). One per_attempt of slack on top of
    // a single attempt absorbs an initial slow upstream without dragging
    // the boot.
    let budget = if probe_request {
        per_attempt.saturating_mul(2)
    } else {
        Duration::from_secs(state.config.proxy.play_budget_secs)
    };
    let started = Instant::now();
    let started_wall = time::OffsetDateTime::now_utc();

    let mut last_err: Option<String> = None;
    let mut tried = 0usize;
    let mut attempts: Vec<PlayAttempt> = Vec::new();

    info!(
        play = %play_id,
        channel = %channel.key,
        candidates = candidates.len(),
        "play start",
    );

    for (idx, cand) in candidates.iter().enumerate() {
        let elapsed = started.elapsed();
        if elapsed >= budget {
            warn!(play = %play_id, channel = %channel.key, after_attempts = idx, "play budget exhausted");
            break;
        }
        let remaining = budget.saturating_sub(elapsed);
        let attempt_timeout = per_attempt.min(remaining);
        tried += 1;
        let attempt_start = Instant::now();
        match tokio::time::timeout(
            attempt_timeout,
            fetch_and_rewrite_playlist(
                &state,
                &channel,
                cand,
                &public_base,
                !probe_request,
            ),
        )
        .await
        {
            Ok(Ok(resp)) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                if !probe_request {
                    state.blacklist.note_url_succeeded(&channel.key, &cand.url);
                    // Persist (pid → upstream) for the feedback endpoint to consume
                    // when this client later reports success/failure. Only meaningful
                    // when the client passed a pid — otherwise feedback falls back
                    // to LKG-based blame.
                    if let Some(pid) = client_pid.as_deref() {
                        state.play_sessions.note(pid, &channel.key, &cand.url);
                    }
                    schedule_opportunistic_validation(
                        Arc::clone(&state),
                        channel.key.clone(),
                        &candidates,
                        idx,
                    );
                }
                attempts.push(PlayAttempt {
                    host: cand.host.clone(),
                    url: cand.url.clone(),
                    elapsed_ms,
                    outcome: AttemptOutcome::Ok,
                });
                info!(
                    play = %play_id,
                    channel = %channel.key,
                    host = %cand.host,
                    attempt = tried,
                    elapsed_ms,
                    "play ok"
                );
                if !probe_request {
                    state.play_log.record(PlayEvent {
                        id: play_id.clone(),
                        started: started_wall,
                        channel: channel.key.clone(),
                        catchup: false,
                        total_ms: started.elapsed().as_millis() as u64,
                        candidates_total: candidates.len(),
                        succeeded: true,
                        error: None,
                        attempts,
                    });
                }
                return Ok(resp);
            }
            Ok(Err(e)) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                let reason = e.to_string();
                warn!(
                    play = %play_id,
                    channel = %channel.key,
                    host = %cand.host,
                    url = %cand.url,
                    elapsed_ms,
                    error = %reason,
                    "playlist fetch failed",
                );
                if !probe_request {
                    state.blacklist.mark_failed(&cand.url);
                }
                attempts.push(PlayAttempt {
                    host: cand.host.clone(),
                    url: cand.url.clone(),
                    elapsed_ms,
                    outcome: AttemptOutcome::Err { reason: reason.clone() },
                });
                last_err = Some(reason);
                continue;
            }
            Err(_) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                warn!(
                    play = %play_id,
                    channel = %channel.key,
                    host = %cand.host,
                    url = %cand.url,
                    elapsed_ms,
                    "playlist fetch timed out",
                );
                if !probe_request {
                    state.blacklist.mark_failed(&cand.url);
                }
                attempts.push(PlayAttempt {
                    host: cand.host.clone(),
                    url: cand.url.clone(),
                    elapsed_ms,
                    outcome: AttemptOutcome::Timeout,
                });
                last_err = Some(format!("timeout after {attempt_timeout:?}"));
                continue;
            }
        }
    }

    let err_text = format!(
        "all {tried}/{total} candidates failed for {channel} (last: {err})",
        total = candidates.len(),
        channel = channel.key,
        err = last_err.as_deref().unwrap_or("budget exhausted"),
    );
    warn!(play = %play_id, channel = %channel.key, attempts = tried, "play failed: all candidates exhausted");
    if !probe_request {
        state.play_log.record(PlayEvent {
            id: play_id.clone(),
            started: started_wall,
            channel: channel.key.clone(),
            catchup: false,
            total_ms: started.elapsed().as_millis() as u64,
            candidates_total: candidates.len(),
            succeeded: false,
            error: Some(last_err.unwrap_or_else(|| "budget exhausted".into())),
            attempts,
        });
    }
    Err((StatusCode::BAD_GATEWAY, err_text))
}

fn build_candidates(state: &AppState, channel: &CanonicalChannel) -> Vec<Candidate> {
    let alive = state.hosts.alive_hosts_ranked();
    let mut fresh: Vec<Candidate> = Vec::new();
    let mut demoted: Vec<Candidate> = Vec::new();

    // Per-source emission order (within score-desc sources):
    //   1. Primary candidate: origin_host × stream_id — the host that actually
    //      reported this stream in the catalog. Tried first because it's the
    //      one we know has it.
    //   2. Speculative candidates: other alive hosts × stream_id — only useful
    //      when the provider replicates stream_ids across hosts. They may 404
    //      otherwise; that's fine, they get demoted and eventually filtered.
    // After enumeration, dedupe by URL (different sources can collide on the
    // same speculative URL) preserving first-seen order so primaries always
    // win over speculatives.
    for src in &channel.sources {
        if let Some(direct) = &src.direct_source {
            // Radio: URL given verbatim by `direct_source` — one candidate
            // per source, no host fanout.
            if state.blacklist.is_url_failed(direct) {
                continue;
            }
            let host = derive_host(direct).unwrap_or_default();
            if !host.is_empty() && state.blacklist.is_host_bad(&host) {
                continue;
            }
            let cand = Candidate {
                url: direct.clone(),
                host,
                stream_id: src.stream_id,
            };
            if state.blacklist.is_url_demoted(direct) {
                demoted.push(cand);
            } else {
                fresh.push(cand);
            }
            continue;
        }
        // Xtream TV source. Emit primary first, then speculatives.
        let primary_host = src.origin_host.as_str();
        if !primary_host.is_empty()
            && alive.iter().any(|h| h == primary_host)
            && !state.blacklist.is_host_bad(primary_host)
        {
            push_xtream_candidate(state, primary_host, src.stream_id, &mut fresh, &mut demoted);
        }
        for host in &alive {
            if host == primary_host {
                continue;
            }
            if state.blacklist.is_host_bad(host) {
                continue;
            }
            push_xtream_candidate(state, host, src.stream_id, &mut fresh, &mut demoted);
        }
    }

    // Dedupe by URL — preserve order so primaries stay ahead of speculatives.
    dedup_preserving_order(&mut fresh);
    dedup_preserving_order(&mut demoted);
    // Any URL that appears in `fresh` shouldn't *also* live in `demoted` (would
    // cause us to retry the same URL twice in a single request).
    {
        let fresh_urls: std::collections::HashSet<&str> =
            fresh.iter().map(|c| c.url.as_str()).collect();
        demoted.retain(|c| !fresh_urls.contains(c.url.as_str()));
    }

    // Measurement-driven rank. Snapshot the play log once (not per-candidate)
    // so success_score is O(candidates × history) rather than O(candidates²).
    // Stable sort preserves the existing host-latency tie-break order for
    // rank-equal candidates.
    let log_snap = state.play_log.snapshot();
    fresh.sort_by(|a, b| {
        let ka = source_rank_key(a.stream_id, &a.host, &state.measured, &log_snap);
        let kb = source_rank_key(b.stream_id, &b.host, &state.measured, &log_snap);
        kb.cmp(&ka)
    });

    // Last-known-good promotion. LKG only wins when its measurement is at
    // least as good as the current top candidate — otherwise the
    // measurement-driven ranking is preferred. Without this check, an old
    // LKG pinned to an unmeasured host (set before the bootstrap sweep
    // populated samples) would override the new ranking forever; the plan
    // calls for clear_last_known_good() after sweep completion, but with
    // max_connections=1 the sweep may not finish cleanly, so we need this
    // belt-and-braces too.
    if let Some(lkg) = state.blacklist.last_known_good(&channel.key) {
        let demoted_lkg = state.blacklist.is_url_demoted(&lkg);
        if !demoted_lkg && !state.blacklist.is_url_failed(&lkg) {
            let lkg_host = derive_host(&lkg).unwrap_or_default();
            let lkg_stream_id = stream_id_from_source_url(&lkg).unwrap_or(0);
            let lkg_rank =
                source_rank_key(lkg_stream_id, &lkg_host, &state.measured, &log_snap);
            let top_rank = fresh.first().map(|c| {
                source_rank_key(c.stream_id, &c.host, &state.measured, &log_snap)
            });
            // LKG promoted if competitive with rank-key top, or if no
            // candidates exist (then LKG is all we have).
            let lkg_competitive = match top_rank {
                Some(top) => lkg_rank >= top,
                None => true,
            };
            if lkg_competitive {
                if let Some(pos) = fresh.iter().position(|c| c.url == lkg) {
                    let item = fresh.remove(pos);
                    fresh.insert(0, item);
                } else {
                    fresh.insert(0, Candidate { url: lkg, host: lkg_host, stream_id: lkg_stream_id });
                }
            }
        }
    }

    fresh.extend(demoted);

    // If the blacklist filtered everything out, fall back to the unfiltered
    // candidate set. The blacklist is a hint, not a hard rule — failing the
    // request without trying anything is worse than probing a possibly-stale
    // entry. If they really are all dead, the attempt loop in `play_playlist`
    // returns whatever error within its budget.
    if fresh.is_empty()
        && (!alive.is_empty() || channel.sources.iter().any(|s| s.direct_source.is_some()))
    {
        for src in &channel.sources {
            if let Some(direct) = &src.direct_source {
                let host = derive_host(direct).unwrap_or_default();
                fresh.push(Candidate {
                    url: direct.clone(),
                    host,
                    stream_id: src.stream_id,
                });
            } else if !src.origin_host.is_empty() {
                let url = state.xtream.stream_url(&src.origin_host, src.stream_id, "m3u8");
                fresh.push(Candidate {
                    url,
                    host: src.origin_host.clone(),
                    stream_id: src.stream_id,
                });
            } else {
                // No origin_host (legacy data) — fall back to fanning across
                // alive hosts. Should only happen during the cold-start
                // window between probe and first multi-host catalog refresh.
                for host in &alive {
                    let url = state.xtream.stream_url(host, src.stream_id, "m3u8");
                    fresh.push(Candidate {
                        url,
                        host: host.clone(),
                        stream_id: src.stream_id,
                    });
                }
            }
        }
        dedup_preserving_order(&mut fresh);
    }

    fresh
}

// --- Rank-key helpers ------------------------------------------------------
//
// Lexicographic comparison key for sorting candidates. Bigger is better.
// Slots (descending priority):
//   0. measured marker (1 if we have a measurement, 0 otherwise)
//   1. success_bucket  — history-aware reliability for this (stream_id, host)
//   2. hdr_rank        — HDR ahead of bpp (TV is OLED)
//   3. bpp_bucket      — bitrate-per-pixel coarse bucket (starved 1080p
//                        loses to well-fed 720p; same-quality streams tie)
//   4. pixels          — resolution as in-bucket tiebreaker
//   5. codec_rank      — av1 > hevc > h264 > mpeg2
//   6. fps_rank        — 50/60 > 25/30
//   7. raw_kbps        — fine-grained bitrate tiebreaker so two equally-
//                        encoded streams on different hosts don't fall back
//                        to stable-sort / catalog order
type RankKey = (i32, i32, i32, i32, i64, i32, i32, i32);

fn hdr_rank(pix_fmt: Option<&str>, transfer: Option<&str>) -> i32 {
    let ten_bit = matches!(pix_fmt, Some(p) if p.contains("10"));
    let hdr = matches!(transfer, Some("smpte2084") | Some("arib-std-b67"));
    match (ten_bit, hdr) {
        (_, true) => 2,
        (true, _) => 1,
        _ => 0,
    }
}

fn bpp_bucket(kbps: Option<u32>, w: u32, h: u32) -> i32 {
    let pixels_k = (w as u64 * h as u64) / 1000;
    if pixels_k == 0 {
        return -1;
    }
    match kbps {
        Some(b) => {
            // bpp in hundredths of kbps/kpx (avoids floats; integer math).
            let bpp_hundredths = (b as u64 * 100) / pixels_k;
            match bpp_hundredths {
                x if x >= 300 => 4, // ~3.0 kbps/kpx — well-fed
                x if x >= 200 => 3,
                x if x >= 100 => 2,
                x if x >= 50 => 1,
                _ => 0, // starved
            }
        }
        None => -1,
    }
}

fn codec_rank(c: Option<&str>) -> i32 {
    match c {
        Some("av1") => 4,
        Some("hevc") | Some("h265") => 3,
        Some("h264") => 2,
        Some("mpeg2video") => 1,
        _ => 0,
    }
}

fn fps_rank(f: Option<f32>) -> i32 {
    match f {
        Some(v) if v >= 48.0 => 2,
        Some(v) if v >= 24.0 => 1,
        _ => 0,
    }
}

/// Per-(stream_id, host) success rate derived from the play log. Recent
/// attempts weighted more heavily (0.9^i decay). Returns 0.5 for keys with
/// no history so we don't penalise untested candidates.
fn success_score(stream_id: u64, host: &str, log_snap: &[PlayEvent]) -> f32 {
    let mut sum_w = 0.0_f32;
    let mut sum = 0.0_f32;
    let mut decay = 1.0_f32;
    for ev in log_snap {
        for att in &ev.attempts {
            if att.host != host {
                continue;
            }
            if stream_id_from_source_url(&att.url) != Some(stream_id) {
                continue;
            }
            let v = match att.outcome {
                AttemptOutcome::Ok => 1.0,
                AttemptOutcome::Err { .. } | AttemptOutcome::Timeout => 0.0,
            };
            sum += v * decay;
            sum_w += decay;
            decay *= 0.9;
        }
    }
    if sum_w == 0.0 {
        0.5
    } else {
        sum / sum_w
    }
}

fn success_bucket(score: f32) -> i32 {
    (score * 10.0).round() as i32 // 0..=10
}

fn source_rank_key(
    stream_id: u64,
    host: &str,
    measured: &MeasuredStore,
    log_snap: &[PlayEvent],
) -> RankKey {
    let success = success_bucket(success_score(stream_id, host, log_snap));
    match measured.get(stream_id, host) {
        Some(q) => {
            let pix_fmt_10bit = q.pix_fmt.as_deref().map(|p| p.contains("10")).unwrap_or(false);
            let is_hevc = q.codec.as_deref() == Some("hevc");
            let hdr_raw = hdr_rank(q.pix_fmt.as_deref(), q.color_transfer.as_deref());
            // If TV can't decode HEVC main10, treat 10-bit HEVC as 8-bit SDR.
            let hdr = if !TV_DECODES_HEVC_MAIN10 && is_hevc && pix_fmt_10bit {
                0
            } else {
                hdr_raw
            };
            (
                1, // measured > unmeasured
                success,
                hdr,
                bpp_bucket(q.bitrate_kbps, q.width, q.height),
                (q.width as u64 * q.height as u64) as i64,
                codec_rank(q.codec.as_deref()),
                fps_rank(q.framerate),
                q.bitrate_kbps.unwrap_or(0) as i32,
            )
        }
        None => (0, success, 0, 0, 0, 0, 0, 0),
    }
}

fn push_xtream_candidate(
    state: &AppState,
    host: &str,
    stream_id: u64,
    fresh: &mut Vec<Candidate>,
    demoted: &mut Vec<Candidate>,
) {
    let url = state.xtream.stream_url(host, stream_id, "m3u8");
    if state.blacklist.is_url_failed(&url) {
        return;
    }
    let cand = Candidate {
        url: url.clone(),
        host: host.to_string(),
        stream_id,
    };
    if state.blacklist.is_url_demoted(&url) {
        demoted.push(cand);
    } else {
        fresh.push(cand);
    }
}

fn dedup_preserving_order(v: &mut Vec<Candidate>) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    v.retain(|c| seen.insert(c.url.clone()));
}

async fn fetch_and_rewrite_playlist(
    state: &AppState,
    channel: &CanonicalChannel,
    cand: &Candidate,
    public_base: &str,
    track_failures: bool,
) -> anyhow::Result<Response> {
    debug!("playlist fetch: {} ({})", channel.key, cand.url);
    let resp = state
        .upstream_http
        .get(&cand.url)
        .send()
        .await?
        .error_for_status()?;

    let final_url = resp.url().clone();
    if is_abuse_url(&final_url) {
        anyhow::bail!("upstream redirected to abuse page: {}", final_url);
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let bytes = resp.bytes().await?;
    let body = std::str::from_utf8(&bytes)?;

    if body.starts_with("#EXTM3U") || content_type.contains("mpegurl") || content_type.contains("m3u8") {
        // Xtream providers return HTTP 200 with a 1-segment ENDLIST playlist
        // pointing at a "placeholder" .ts (typically a few seconds of black
        // video) when the actual stream is unavailable — dead stream_id, geo
        // block, account quota, etc. Two detectors cover this:
        //   - `looks_like_placeholder_playlist`: filename-based (`black.ts`,
        //     `offline.ts`, …) — catches the common cases
        //   - `is_placeholder_manifest`: structural (#EXT-X-ENDLIST + ≤2
        //     #EXTINF) — catches the auth-saturated cases where the
        //     filename isn't a known needle (e.g. `media_NNNNN.ts`)
        if looks_like_placeholder_playlist(body) || is_placeholder_manifest(body) {
            anyhow::bail!(
                "upstream returned a placeholder/black playlist (stream unavailable on this source)"
            );
        }
        let rewritten = rewrite_playlist(
            body,
            &final_url,
            public_base,
            &channel.key,
            &cand.url,
            track_failures,
            Some(&cand.host),
        )?;
        let mut response = Response::new(Body::from(rewritten));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(PLAYLIST_CONTENT_TYPE),
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
        if let Ok(v) = HeaderValue::from_str(&cand.url) {
            response.headers_mut().insert("x-upstream", v);
        }
        return Ok(response);
    }

    anyhow::bail!(
        "upstream did not return a playlist (content-type={content_type}, first bytes={:?})",
        &bytes.get(..16)
    )
}

fn rewrite_playlist(
    body: &str,
    playlist_url: &Url,
    public_base: &str,
    channel_key: &str,
    source_url: &str,
    track_failures: bool,
    upstream_host: Option<&str>,
) -> anyhow::Result<String> {
    let base = playlist_url.clone();
    let public_base = public_base.trim_end_matches('/');
    let mut out = String::with_capacity(body.len() + 256);

    // Carry the most-recently-seen #EXTINF duration into the next segment
    // URL's proxy token. Reset on each segment so a token doesn't inherit
    // a stale duration from N lines back.
    let mut pending_duration: Option<f32> = None;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        if trimmed.starts_with('#') {
            if let Some(rest) = trimmed.strip_prefix("#EXTINF:") {
                let dur_str = rest.split(',').next().unwrap_or(rest);
                if let Ok(d) = dur_str.parse::<f32>() {
                    if d > 0.0 {
                        pending_duration = Some(d);
                    }
                }
            }
            if let Some(rewritten) = rewrite_tag_with_uri(
                trimmed,
                &base,
                public_base,
                channel_key,
                source_url,
                track_failures,
                upstream_host,
            ) {
                out.push_str(&rewritten);
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if let Ok(resolved) = base.join(trimmed) {
            out.push_str(&proxy_url(
                public_base,
                resolved.as_str(),
                channel_key,
                source_url,
                track_failures,
                pending_duration,
                upstream_host,
            ));
            out.push('\n');
            pending_duration = None;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

fn rewrite_tag_with_uri(
    line: &str,
    base: &Url,
    public_base: &str,
    channel_key: &str,
    source_url: &str,
    track_failures: bool,
    upstream_host: Option<&str>,
) -> Option<String> {
    let uri_marker = "URI=\"";
    let idx = line.find(uri_marker)?;
    let start = idx + uri_marker.len();
    let rel_end = line[start..].find('"')?;
    let raw = &line[start..start + rel_end];
    let resolved = base.join(raw).ok()?;
    // No duration to attach for URI=-tag references — these are usually init
    // segments, subtitle tracks, etc. Bitrate measurement only applies to
    // media segments (the lines after #EXTINF).
    let new_uri = proxy_url(
        public_base,
        resolved.as_str(),
        channel_key,
        source_url,
        track_failures,
        None,
        upstream_host,
    );
    let mut s = String::with_capacity(line.len() + new_uri.len());
    s.push_str(&line[..start]);
    s.push_str(&new_uri);
    s.push_str(&line[start + rel_end..]);
    Some(s)
}

fn proxy_url(
    public_base: &str,
    absolute_upstream: &str,
    channel_key: &str,
    source_url: &str,
    track_failures: bool,
    duration: Option<f32>,
    host: Option<&str>,
) -> String {
    let payload = SegmentToken {
        u: absolute_upstream.to_string(),
        p: Some(source_url.to_string()),
        c: Some(channel_key.to_string()),
        probe: !track_failures,
        d: duration,
        h: host.map(|s| s.to_string()),
    };
    let json = serde_json::to_string(&payload).unwrap_or_else(|_| absolute_upstream.to_string());
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
    let ext = upstream_ext(absolute_upstream).unwrap_or_else(|| "ts".to_string());
    format!("{public_base}/seg/{token}.{ext}")
}

fn upstream_ext(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let last = path.rsplit('/').next()?;
    let dot = last.rfind('.')?;
    let ext: String = last[dot + 1..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_lowercase())
    }
}

fn is_abuse_url(u: &Url) -> bool {
    let host = u.host_str().unwrap_or("");
    host.contains("cloudflare-terms-of-service-abuse") || host.contains("abuse")
}

/// True if the playlist body looks like the Xtream "stream unavailable"
/// placeholder — a short ENDLIST playlist whose segments point at a
/// well-known filler filename (`black.ts`, `offline.ts`, …). The provider
/// returns these instead of HTTP errors when an account/IP can't reach the
/// real stream, which makes them indistinguishable from real playlists at
/// the HTTP layer. Caller treats a match as a fetch failure so the candidate
/// loop falls through to the next source.
fn looks_like_placeholder_playlist(body: &str) -> bool {
    // Match on filename component so we don't false-positive on legitimate
    // streams whose path happens to contain one of these as a substring.
    // Lowercased once per body — segment URLs are short and case-insensitive
    // on the parts we care about.
    static NEEDLES: &[&str] = &[
        "/black.ts",
        "/offline.ts",
        "/no_signal.ts",
        "/no-signal.ts",
        "/nosignal.ts",
        "/notavailable.ts",
        "/not-available.ts",
        "/comingsoon.ts",
        "/coming-soon.ts",
        "/closed.ts",
    ];
    for line in body.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let lower = l.to_ascii_lowercase();
        if NEEDLES.iter().any(|needle| lower.contains(needle)) {
            return true;
        }
    }
    false
}

fn derive_host(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    Some(format!("{}://{}", u.scheme(), u.host_str()?))
}

fn decode_segment_token(token: &str) -> Result<SegmentToken, (StatusCode, String)> {
    let token = token
        .split('?')
        .next()
        .unwrap_or(token)
        .trim_end_matches(".ts")
        .trim_end_matches(".m4s")
        .trim_end_matches(".m3u8")
        .trim_end_matches(".aac")
        .trim_end_matches(".mp3");
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad segment token".into()))?;
    if let Ok(parsed) = serde_json::from_slice::<SegmentToken>(&decoded) {
        if !parsed.u.is_empty() {
            return Ok(parsed);
        }
    }
    let upstream = std::str::from_utf8(&decoded)
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad segment token (utf8)".into()))?
        .to_string();
    Ok(SegmentToken {
        u: upstream,
        p: None,
        c: None,
        probe: false,
        d: None,
        h: None,
    })
}

pub async fn proxy_segment(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let segment = decode_segment_token(&token)?;
    let upstream = segment.u.clone();

    debug!("segment fetch: {}", upstream);

    let mut req = state.upstream_http.get(&upstream);
    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(v) = range.to_str() {
            req = req.header(header::RANGE, v);
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("segment upstream error: {} → {}", upstream, e);
            mark_segment_failure(&state, &segment);
            return Err((StatusCode::BAD_GATEWAY, format!("upstream error: {e}")));
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() && !status.is_redirection() {
        mark_segment_failure(&state, &segment);
        warn!("segment upstream returned status {}: {}", status, upstream);
        return Err((status, format!("upstream status {}", status.as_u16())));
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let upstream_path = upstream.split('?').next().unwrap_or(&upstream);
    let final_url = resp.url().clone();
    let is_playlist = content_type.contains("mpegurl")
        || content_type.contains("m3u8")
        || upstream_path.ends_with(".m3u8");
    let is_ts = content_type.contains("mp2t") || upstream_path.ends_with(".ts");
    let stream_id = segment.p.as_deref().and_then(stream_id_from_source_url);

    // Nested HLS: the master playlist references a chunklist `.m3u8` which we
    // proxied through `/seg/<token>.m3u8`. When the browser now fetches that,
    // the response is still a playlist — needs its inner segment URLs rewritten
    // too. Reuse the same `rewrite_playlist` machinery used for the master,
    // recurring once until it bottoms out at real `.ts` (or `.aac`) segments.
    if is_playlist {
        let public_base = request_base_url(&headers, state.config.public_base_url.as_deref());
        let channel_key = segment.c.clone().unwrap_or_default();
        let source_url = segment.p.clone().unwrap_or_else(|| upstream.clone());
        let upstream_headers = resp.headers().clone();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("nested playlist body read failed: {} → {}", upstream, e);
                mark_segment_failure(&state, &segment);
                return Err((StatusCode::BAD_GATEWAY, format!("body read: {e}")));
            }
        };
        let body = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                // Not UTF-8 — pass through; the player will surface whatever
                // error the content type implies.
                return passthrough_response(status, &upstream_headers, bytes);
            }
        };
        let rewritten = match rewrite_playlist(
            body,
            &final_url,
            &public_base,
            &channel_key,
            &source_url,
            !segment.probe,
            segment.h.as_deref(),
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("nested playlist rewrite failed: {} → {}", upstream, e);
                return Err((StatusCode::BAD_GATEWAY, format!("rewrite: {e}")));
            }
        };
        let bytes_out = rewritten.into_bytes();
        let mut builder = Response::builder().status(status);
        builder = builder.header(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE);
        builder = builder.header(header::CACHE_CONTROL, "no-store");
        builder = builder.header(header::CONTENT_LENGTH, bytes_out.len());
        return builder
            .body(Body::from(bytes_out))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("body build error: {e}")));
    }

    // For MPEG-TS segments with a known stream_id, buffer the body so we can
    // (a) classify the codec / detect DVB subtitle PIDs and (b) optionally
    // strip those PIDs before forwarding. Other content types (m4s, init
    // segments) pass straight through.
    if is_ts && stream_id.is_some() {
        let upstream_headers = resp.headers().clone();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("segment body read failed: {} → {}", upstream, e);
                mark_segment_failure(&state, &segment);
                return Err((StatusCode::BAD_GATEWAY, format!("body read: {e}")));
            }
        };
        // Per-play bitrate observation: before handle_ts_segment mutates
        // the bytes (subtitle PID strip changes the size), record the
        // observed kbps for the per-(stream_id, host) EWMA. Skip probe-mode
        // segments so the client capability probe never shapes measurements.
        if !segment.probe {
            if let (Some(sid), Some(d), Some(h)) = (stream_id, segment.d, segment.h.as_deref()) {
                if d > 0.0 && bytes.len() > 0 {
                    let kbps = bytes.len() as f64 * 8.0 / 1000.0 / d as f64;
                    state.per_play.note_segment_kbps(sid, h, kbps as f32);
                }
            }
        }
        let processed = handle_ts_segment(&state, stream_id.unwrap(), &bytes, &segment);
        // After handle_ts_segment has run (and possibly cached the
        // classification), push per-play metadata into the accumulator.
        // Idempotent — subsequent calls just refresh last_activity.
        if !segment.probe {
            if let (Some(sid), Some(h)) = (stream_id, segment.h.as_deref()) {
                if let Some(c) = state.classifier.get(sid) {
                    state.per_play.note_classification(
                        sid,
                        h,
                        c.width,
                        c.height,
                        c.codec_string(),
                        c.pix_fmt.clone(),
                        c.color_transfer.clone(),
                        c.framerate,
                    );
                }
            }
        }
        let mut builder = Response::builder().status(status);
        for h in [
            header::CONTENT_TYPE,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::CACHE_CONTROL,
            header::ETAG,
            header::LAST_MODIFIED,
        ] {
            if let Some(v) = upstream_headers.get(&h) {
                builder = builder.header(&h, v);
            }
        }
        builder = builder.header(header::CONTENT_LENGTH, processed.len());
        return builder
            .body(Body::from(processed))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("body build error: {e}")));
    }

    let mut builder = Response::builder().status(status);
    for h in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CACHE_CONTROL,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        if let Some(v) = resp.headers().get(&h) {
            builder = builder.header(&h, v);
        }
    }
    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);
    builder
        .body(body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("body build error: {e}")))
}

fn passthrough_response(
    status: StatusCode,
    upstream_headers: &reqwest::header::HeaderMap,
    bytes: Bytes,
) -> Result<Response, (StatusCode, String)> {
    let mut builder = Response::builder().status(status);
    for h in [
        header::CONTENT_TYPE,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CACHE_CONTROL,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        if let Some(v) = upstream_headers.get(&h) {
            builder = builder.header(&h, v);
        }
    }
    builder = builder.header(header::CONTENT_LENGTH, bytes.len());
    builder
        .body(Body::from(bytes))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("body build error: {e}")))
}

fn handle_ts_segment(
    state: &AppState,
    stream_id: u64,
    bytes: &Bytes,
    segment: &SegmentToken,
) -> Vec<u8> {
    let classification = match state.classifier.get(stream_id) {
        // Partial cache: first segment parsed PMT/PAT but the SPS NAL wasn't
        // present in it (segment started mid-GOP, common at random join
        // points). Re-attempt SPS extraction on this segment so per-play
        // samples can pick up width/height/colour metadata. Update the cache
        // only if the new attempt added information.
        Some(c) if c.width.is_none() => {
            match classify_ts_chunk(bytes) {
                Some(new_c) if new_c.width.is_some() => {
                    debug!(
                        stream_id = stream_id,
                        w = ?new_c.width,
                        h = ?new_c.height,
                        codec = ?new_c.codec_string(),
                        "classifier: SPS extracted on retry",
                    );
                    state.classifier.set(stream_id, new_c.clone());
                    new_c
                }
                _ => c,
            }
        }
        Some(c) => c,
        None => match classify_ts_chunk(bytes) {
            Some(c) => {
                info!(
                    stream_id = stream_id,
                    codec = ?c.video_codec,
                    pmt = ?c.pmt_pid,
                    video = ?c.video_pid,
                    pcr = ?c.pcr_pid,
                    subs = c.subtitle_pids.len(),
                    w = ?c.width,
                    h = ?c.height,
                    "classified stream"
                );
                state.classifier.set(stream_id, c.clone());
                c
            }
            None => return bytes.to_vec(),
        },
    };
    let pids = classification.strippable_subtitle_pids();
    let Some(pmt_pid) = classification.pmt_pid else {
        return bytes.to_vec();
    };
    if pids.is_empty() {
        // No strippable subs. But if the classifier found subtitle PIDs we
        // *can't* strip (because PCR rides on the same PID), demote the source
        // — the webOS demuxer freezes on DVB subs and we have no way to fix
        // it in flight. Better to send the next candidate next time. The
        // demote also affects LKG handling so this same URL won't be the
        // preferred pick on the next play.
        if !classification.subtitle_pids.is_empty() && !segment.probe {
            if let Some(source_url) = segment.p.as_deref() {
                if !state.blacklist.is_url_demoted(source_url) {
                    warn!(
                        stream_id = stream_id,
                        url = %source_url,
                        "DVB subs collide with PCR PID; demoting source — webOS demuxer would stall"
                    );
                    state.blacklist.demote_url(source_url);
                    if let Some(channel_key) = segment.c.as_deref() {
                        state.blacklist.drop_last_known_good_if_matches(channel_key, source_url);
                    }
                }
            }
        }
        return bytes.to_vec();
    }
    strip_subtitle_pids(bytes, pmt_pid, &pids)
}

fn stream_id_from_source_url(url: &str) -> Option<u64> {
    if let Some(after) = url.split("/live/").nth(1) {
        let parts: Vec<&str> = after.split('/').collect();
        if let Some(last) = parts.get(2) {
            if let Some(id) = last.split('.').next() {
                if let Ok(n) = id.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    if let Some(after) = url.split("/timeshift/").nth(1) {
        let parts: Vec<&str> = after.split('/').collect();
        if let Some(last) = parts.get(4) {
            if let Some(id) = last.split('.').next() {
                if let Ok(n) = id.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn mark_segment_failure(state: &AppState, segment: &SegmentToken) {
    if segment.probe {
        return;
    }
    state.blacklist.mark_failed(&segment.u);
    if let Some(source_url) = segment.p.as_deref() {
        state.blacklist.mark_failed(source_url);
        if let Some(channel_key) = segment.c.as_deref() {
            if let Some(lkg) = state.blacklist.last_known_good(channel_key) {
                if lkg == source_url {
                    state.blacklist.drop_last_known_good(channel_key);
                }
            }
        }
    }
}

/// Walk one segment-deep into a master playlist and verify the segment is
/// fetchable. Returns Ok on a 2xx response with a non-empty body; Err
/// otherwise. Handles one level of nested playlist (master → chunklist →
/// segment), which is what this provider serves. Range-limits the segment
/// fetch to 512 bytes so we don't pull a full TS chunk just to check liveness.
async fn validate_one_segment(
    state: &AppState,
    playlist_url: &Url,
    body: &Bytes,
    timeout: Duration,
) -> Result<(), String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("utf8: {e}"))?;
    let first_uri = pick_first_uri_in_playlist(text).ok_or("no URI in playlist".to_string())?;
    let resolved = playlist_url
        .join(&first_uri)
        .map_err(|e| format!("resolve {first_uri}: {e}"))?;

    let resp = tokio::time::timeout(
        timeout,
        state
            .upstream_http
            .get(resolved.as_str())
            .header(reqwest::header::RANGE, "bytes=0-511")
            .send(),
    )
    .await
    .map_err(|_| "segment fetch timed out".to_string())?
    .map_err(|e| format!("segment request: {e}"))?;
    let status = resp.status();
    if !status.is_success() && !status.is_redirection() {
        return Err(format!("segment status {}", status.as_u16()));
    }
    let final_url = resp.url().clone();
    if is_abuse_url(&final_url) {
        return Err("segment redirected to abuse page".into());
    }
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    // Nested playlist: chunklist response. Recurse one level — fetch the first
    // segment URI from it. Stop after one recursion; deeper nests aren't
    // observed in practice and would risk a probing loop.
    if content_type.contains("mpegurl")
        || content_type.contains("m3u8")
        || final_url.path().ends_with(".m3u8")
    {
        let inner_bytes = tokio::time::timeout(timeout, resp.bytes())
            .await
            .map_err(|_| "chunklist body timed out".to_string())?
            .map_err(|e| format!("chunklist body: {e}"))?;
        let inner_text = std::str::from_utf8(&inner_bytes).map_err(|e| format!("chunklist utf8: {e}"))?;
        let inner_uri = pick_first_uri_in_playlist(inner_text)
            .ok_or("no URI in chunklist".to_string())?;
        let inner_resolved = final_url
            .join(&inner_uri)
            .map_err(|e| format!("resolve inner {inner_uri}: {e}"))?;
        let inner_resp = tokio::time::timeout(
            timeout,
            state
                .upstream_http
                .get(inner_resolved.as_str())
                .header(reqwest::header::RANGE, "bytes=0-511")
                .send(),
        )
        .await
        .map_err(|_| "inner segment fetch timed out".to_string())?
        .map_err(|e| format!("inner segment: {e}"))?;
        let inner_status = inner_resp.status();
        if !inner_status.is_success() && !inner_status.is_redirection() {
            return Err(format!("inner segment status {}", inner_status.as_u16()));
        }
        return Ok(());
    }

    // Real segment: drain a few bytes to confirm the body actually transfers.
    let _ = tokio::time::timeout(timeout, resp.bytes()).await;
    Ok(())
}

fn pick_first_uri_in_playlist(body: &str) -> Option<String> {
    for line in body.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        return Some(l.to_string());
    }
    None
}

fn schedule_opportunistic_validation(
    state: Arc<AppState>,
    channel_key: String,
    candidates: &[Candidate],
    served_idx: usize,
) {
    let count = state.config.proxy.opportunistic_validate_count;
    if count == 0 || candidates.len() <= served_idx + 1 {
        return;
    }
    let to_validate: Vec<Candidate> = candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != served_idx)
        .map(|(_, c)| c.clone())
        .take(count)
        .collect();
    if to_validate.is_empty() {
        return;
    }
    let timeout = Duration::from_secs(state.config.proxy.opportunistic_validate_timeout_secs);
    tokio::spawn(async move {
        for cand in to_validate {
            if state.blacklist.is_url_failed(&cand.url) {
                continue;
            }
            let fut = state
                .upstream_http
                .get(&cand.url)
                .send();
            match tokio::time::timeout(timeout, fut).await {
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    let final_url = resp.url().clone();
                    if !status.is_success() && !status.is_redirection() {
                        warn!(
                            channel = %channel_key,
                            url = %cand.url,
                            status = %status,
                            "opportunistic validation: bad status"
                        );
                        state.blacklist.mark_failed(&cand.url);
                        continue;
                    }
                    if is_abuse_url(&final_url) {
                        warn!(
                            channel = %channel_key,
                            url = %cand.url,
                            "opportunistic validation: abuse redirect"
                        );
                        state.blacklist.mark_failed(&cand.url);
                        continue;
                    }
                    match resp.bytes().await {
                        Ok(bytes) => {
                            let head = std::str::from_utf8(bytes.get(..7).unwrap_or(&[])).unwrap_or("");
                            let body_str = std::str::from_utf8(&bytes).unwrap_or("");
                            if head.starts_with("#EXTM3U")
                                && looks_like_placeholder_playlist(body_str)
                            {
                                warn!(
                                    channel = %channel_key,
                                    url = %cand.url,
                                    "opportunistic validation: placeholder playlist"
                                );
                                state.blacklist.mark_failed(&cand.url);
                            } else if head.starts_with("#EXTM3U") {
                                // Master playlist is OK; now verify segments are
                                // actually fetchable. A surprisingly common
                                // failure mode is "playlist responds but every
                                // segment URL 502s" — the URL-only validation
                                // missed it. Follow the chunklist + one segment.
                                match validate_one_segment(&state, &final_url, &bytes, timeout).await {
                                    Ok(()) => {
                                        debug!(
                                            channel = %channel_key,
                                            url = %cand.url,
                                            "opportunistic validation: playlist + segment ok"
                                        );
                                    }
                                    Err(reason) => {
                                        warn!(
                                            channel = %channel_key,
                                            url = %cand.url,
                                            reason = %reason,
                                            "opportunistic validation: playlist ok but segment failed"
                                        );
                                        state.blacklist.mark_failed(&cand.url);
                                    }
                                }
                            } else {
                                warn!(
                                    channel = %channel_key,
                                    url = %cand.url,
                                    "opportunistic validation: not a playlist"
                                );
                                state.blacklist.mark_failed(&cand.url);
                            }
                        }
                        Err(e) => {
                            warn!(
                                channel = %channel_key,
                                url = %cand.url,
                                err = %e,
                                "opportunistic validation: body read failed"
                            );
                            state.blacklist.mark_failed(&cand.url);
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(
                        channel = %channel_key,
                        url = %cand.url,
                        err = %e,
                        "opportunistic validation: request error"
                    );
                    state.blacklist.mark_failed(&cand.url);
                }
                Err(_) => {
                    warn!(
                        channel = %channel_key,
                        url = %cand.url,
                        "opportunistic validation: timeout"
                    );
                    state.blacklist.mark_failed(&cand.url);
                }
            }
        }
    });
}

pub async fn play_legacy() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "use /play/<channel-key>.m3u8")
}

// ---- catch-up ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CatchupRequest {
    start: OffsetDateTime,
    explicit_duration_min: Option<u32>,
}

pub fn parse_catchup_params(p: &PlayParams) -> Result<Option<CatchupRequest>, (StatusCode, String)> {
    parse_catchup_params_at(p, OffsetDateTime::now_utc())
}

pub fn parse_catchup_params_at(
    p: &PlayParams,
    now: OffsetDateTime,
) -> Result<Option<CatchupRequest>, (StatusCode, String)> {
    if p.at.is_none() && p.from.is_none() && p.duration.is_none() {
        return Ok(None);
    }
    if p.at.is_some() && p.from.is_some() {
        return Err((StatusCode::BAD_REQUEST, "use either at or from, not both".into()));
    }

    let start = if let Some(s) = p.at.as_deref() {
        let parsed = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "at must be RFC3339 with offset (e.g. 2026-05-12T20:00:00Z)".into(),
                )
            })?;
        parsed.to_offset(time::UtcOffset::UTC)
    } else if let Some(s) = p.from.as_deref() {
        let secs: i64 = s.parse().ok().filter(|n| *n >= 0).ok_or((
            StatusCode::BAD_REQUEST,
            "from must be a non-negative integer (seconds)".into(),
        ))?;
        now - time::Duration::seconds(secs)
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            "duration requires at or from".into(),
        ));
    };

    let explicit_duration_min = match p.duration.as_deref() {
        None => None,
        Some(s) => Some(s.parse::<u32>().ok().filter(|n| *n > 0).ok_or((
            StatusCode::BAD_REQUEST,
            "duration must be a positive integer (minutes)".into(),
        ))?),
    };

    if start >= now {
        return Err((StatusCode::BAD_REQUEST, "at must be in the past".into()));
    }

    Ok(Some(CatchupRequest { start, explicit_duration_min }))
}

async fn catchup_play(
    state: Arc<AppState>,
    channel: CanonicalChannel,
    req: CatchupRequest,
    public_base: &str,
    play_id: &str,
    client_pid: Option<&str>,
) -> Result<Response, (StatusCode, String)> {
    let started = Instant::now();
    let started_wall = time::OffsetDateTime::now_utc();
    let source = channel.pick_archive_source().ok_or((
        StatusCode::NOT_FOUND,
        "channel does not support catch-up".to_string(),
    ))?;

    // tv_archive_duration is set whenever pick_archive_source_for_play returns Some.
    let days = source.tv_archive_duration.unwrap_or(1);
    let window_secs = (days as u64) * 86400;
    let earliest = OffsetDateTime::now_utc()
        - time::Duration::seconds(window_secs as i64);
    if req.start < earliest {
        return Err((
            StatusCode::BAD_REQUEST,
            "start time is outside the catch-up window".into(),
        ));
    }

    let candidate_hosts = archive_candidate_hosts(&state);
    if candidate_hosts.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "catch-up upstream failed: no alive host".into(),
        ));
    }

    let now = OffsetDateTime::now_utc();
    let duration_min = match req.explicit_duration_min {
        Some(d) => {
            let cap = (days as u64) * 24 * 60;
            (d as u64).min(cap).max(1) as u32
        }
        None => {
            let elapsed_min = ((now - req.start).whole_minutes()).max(0) as u64 + 5;
            let cap = (days as u64) * 24 * 60;
            elapsed_min.min(cap).max(1) as u32
        }
    };

    let per_attempt = Duration::from_secs(state.config.proxy.per_attempt_timeout_secs);
    let budget = Duration::from_secs(state.config.proxy.play_budget_secs);
    let mut attempts: Vec<PlayAttempt> = Vec::new();
    let mut last_err: Option<String> = None;
    let total_candidates = candidate_hosts.len();
    info!(
        play = %play_id,
        channel = %channel.key,
        candidates = total_candidates,
        "catchup play start"
    );

    for host in candidate_hosts {
        let elapsed = started.elapsed();
        if elapsed >= budget {
            warn!(play = %play_id, channel = %channel.key, "catchup budget exhausted");
            break;
        }
        let remaining = budget.saturating_sub(elapsed);
        let attempt_timeout = per_attempt.min(remaining);

        let upstream = state.xtream.timeshift_url(&host, source.stream_id, duration_min, req.start);
        debug!(
            channel = %channel.key,
            host = %host,
            stream_id = source.stream_id,
            start = %req.start,
            duration_min = duration_min,
            upstream = %upstream,
            "catch-up playlist fetch"
        );
        let cand = Candidate {
            url: upstream.clone(),
            host: host.clone(),
            stream_id: source.stream_id,
        };
        let attempt_start = Instant::now();
        match tokio::time::timeout(
            attempt_timeout,
            fetch_and_rewrite_playlist(&state, &channel, &cand, public_base, true),
        )
        .await
        {
            Ok(Ok(resp)) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                info!(play = %play_id, channel = %channel.key, host = %cand.host, elapsed_ms, "catchup play ok");
                if let Some(pid) = client_pid {
                    state.play_sessions.note(pid, &channel.key, &cand.url);
                }
                attempts.push(PlayAttempt {
                    host: cand.host.clone(),
                    url: cand.url.clone(),
                    elapsed_ms,
                    outcome: AttemptOutcome::Ok,
                });
                state.play_log.record(PlayEvent {
                    id: play_id.to_string(),
                    started: started_wall,
                    channel: channel.key.clone(),
                    catchup: true,
                    total_ms: started.elapsed().as_millis() as u64,
                    candidates_total: total_candidates,
                    succeeded: true,
                    error: None,
                    attempts,
                });
                return Ok(resp);
            }
            Ok(Err(e)) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                let reason = e.to_string();
                warn!(
                    play = %play_id,
                    channel = %channel.key,
                    host = %cand.host,
                    url = %cand.url,
                    elapsed_ms,
                    error = %reason,
                    "catch-up upstream failed",
                );
                // Catchup is a single-source path; URL-level failure tracking still
                // applies so a permanently-bad host's URLs get demoted/blacklisted
                // like live URLs do.
                state.blacklist.mark_failed(&cand.url);
                attempts.push(PlayAttempt {
                    host: cand.host.clone(),
                    url: cand.url.clone(),
                    elapsed_ms,
                    outcome: AttemptOutcome::Err { reason: reason.clone() },
                });
                last_err = Some(reason);
                continue;
            }
            Err(_) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                warn!(
                    play = %play_id,
                    channel = %channel.key,
                    host = %cand.host,
                    url = %cand.url,
                    elapsed_ms,
                    "catch-up upstream timed out",
                );
                state.blacklist.mark_failed(&cand.url);
                attempts.push(PlayAttempt {
                    host: cand.host.clone(),
                    url: cand.url.clone(),
                    elapsed_ms,
                    outcome: AttemptOutcome::Timeout,
                });
                last_err = Some(format!("timeout after {attempt_timeout:?}"));
                continue;
            }
        }
    }

    let err_text = format!(
        "catch-up upstream failed: {}",
        last_err.as_deref().unwrap_or("no alive host accepted the request"),
    );
    state.play_log.record(PlayEvent {
        id: play_id.to_string(),
        started: started_wall,
        channel: channel.key.clone(),
        catchup: true,
        total_ms: started.elapsed().as_millis() as u64,
        candidates_total: total_candidates,
        succeeded: false,
        error: Some(last_err.unwrap_or_else(|| "no alive host accepted the request".into())),
        attempts,
    });
    Err((StatusCode::BAD_GATEWAY, err_text))
}

/// Ordered list of alive hosts to try for a catch-up request, with the same
/// safety-valve semantics as `build_candidates`: prefer non-blacklisted hosts,
/// but fall back to the blacklisted ones if that's all we have. Returns
/// hosts in latency-ascending order.
fn archive_candidate_hosts(state: &AppState) -> Vec<String> {
    let alive = state.hosts.alive_hosts_ranked();
    let mut fresh: Vec<String> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    for h in alive {
        if state.blacklist.is_host_bad(&h) {
            bad.push(h);
        } else {
            fresh.push(h);
        }
    }
    if fresh.is_empty() {
        // Blacklist filtered everything out — try the blacklisted ones anyway
        // before returning 502. Matches build_candidates' fallback.
        fresh.extend(bad);
    }
    fresh
}

/// pid is opaque to the proxy but ends up in tracing fields, log files and the
/// /admin/recent-plays diagnostic. Allow only short alphanumerics so a hostile
/// client can't inject control characters or pin arbitrarily-large strings into
/// the play_sessions map.
fn sanitize_pid(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xtream::{format_timeshift_start, XtreamClient};

    fn p(at: Option<&str>, from: Option<&str>, dur: Option<&str>) -> PlayParams {
        PlayParams {
            at: at.map(str::to_string),
            from: from.map(str::to_string),
            duration: dur.map(str::to_string),
            probe: false,
            pid: None,
        }
    }

    fn parse_at_fixed(
        params: &PlayParams,
        now_rfc: &str,
    ) -> Result<Option<CatchupRequest>, (StatusCode, String)> {
        let now = OffsetDateTime::parse(now_rfc, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .to_offset(time::UtcOffset::UTC);
        parse_catchup_params_at(params, now)
    }

    #[test]
    fn empty_params_means_live() {
        assert!(parse_catchup_params_at(&p(None, None, None), OffsetDateTime::now_utc())
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_both_at_and_from() {
        let r = parse_at_fixed(
            &p(Some("2026-05-12T20:00:00Z"), Some("600"), None),
            "2026-05-13T00:00:00Z",
        );
        let (code, _) = r.unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_naive_at() {
        // RFC3339 without offset must fail.
        let r = parse_at_fixed(
            &p(Some("2026-05-12T20:00:00"), None, None),
            "2026-05-13T00:00:00Z",
        );
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_future_at() {
        let r = parse_at_fixed(
            &p(Some("2030-01-01T00:00:00Z"), None, None),
            "2026-05-13T00:00:00Z",
        );
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_from_zero_as_not_past() {
        // from=0 → start == now → not strictly past.
        let r = parse_at_fixed(&p(None, Some("0"), None), "2026-05-13T00:00:00Z");
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_from_negative() {
        let r = parse_at_fixed(&p(None, Some("-5"), None), "2026-05-13T00:00:00Z");
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_from_non_integer() {
        let r = parse_at_fixed(&p(None, Some("ten"), None), "2026-05-13T00:00:00Z");
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_zero_duration() {
        let r = parse_at_fixed(
            &p(Some("2026-05-12T20:00:00Z"), None, Some("0")),
            "2026-05-13T00:00:00Z",
        );
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_duration_alone() {
        let r = parse_at_fixed(&p(None, None, Some("60")), "2026-05-13T00:00:00Z");
        assert_eq!(r.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn at_parses_into_utc() {
        let r = parse_at_fixed(
            &p(Some("2026-05-12T22:00:00+02:00"), None, None),
            "2026-05-13T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        // 22:00 in +02:00 == 20:00 UTC.
        assert_eq!(r.start.hour(), 20);
        assert_eq!(r.start.minute(), 0);
        assert_eq!(r.start.offset(), time::UtcOffset::UTC);
        assert!(r.explicit_duration_min.is_none());
    }

    #[test]
    fn from_subtracts_seconds() {
        let r = parse_at_fixed(&p(None, Some("600"), None), "2026-05-13T00:00:00Z")
            .unwrap()
            .unwrap();
        // 600s = 10 min before midnight UTC = 23:50 on prior day.
        assert_eq!(r.start.hour(), 23);
        assert_eq!(r.start.minute(), 50);
    }

    #[test]
    fn explicit_duration_carried() {
        let r = parse_at_fixed(
            &p(Some("2026-05-12T20:00:00Z"), None, Some("180")),
            "2026-05-13T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.explicit_duration_min, Some(180));
    }

    #[test]
    fn timeshift_url_matches_provider_format() {
        let client = XtreamClient::new("USER".into(), "PASS".into(), Duration::from_secs(8)).unwrap();
        let start = OffsetDateTime::parse(
            "2026-05-12T20:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let url = client.timeshift_url("http://cf.host", 386405, 60, start);
        assert_eq!(
            url,
            "http://cf.host/timeshift/USER/PASS/60/2026-05-12:20-00/386405.m3u8"
        );
    }

    #[test]
    fn timeshift_url_converts_offset_to_utc() {
        let client = XtreamClient::new("u".into(), "p".into(), Duration::from_secs(8)).unwrap();
        // 22:00 +02:00 == 20:00 UTC; the timeshift segment must use the UTC value.
        let start = OffsetDateTime::parse(
            "2026-05-12T22:00:00+02:00",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let url = client.timeshift_url("http://h", 1, 5, start);
        assert!(url.contains("/5/2026-05-12:20-00/"), "got: {url}");
    }

    #[test]
    fn format_timeshift_start_zero_pads() {
        let start = OffsetDateTime::parse(
            "2026-01-05T03:07:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(format_timeshift_start(start), "2026-01-05:03-07");
    }

    #[test]
    fn pick_archive_source_prefers_highest_score() {
        use crate::canonical::CanonicalSource;
        use crate::xtream::ChannelKind;
        let ch = CanonicalChannel {
            key: "x".into(),
            name: "X".into(),
            kind: ChannelKind::Tv,
            sources: vec![
                CanonicalSource {
                    stream_id: 1,
                    name: "HD".into(),
                    score: 15,
                    logo: None,
                    tv_archive: true,
                    tv_archive_duration: Some(3),
                    direct_source: None,
                    origin_host: String::new(),
                },
                CanonicalSource {
                    stream_id: 2,
                    name: "4K".into(),
                    score: 30,
                    logo: None,
                    tv_archive: true,
                    tv_archive_duration: Some(3),
                    direct_source: None,
                    origin_host: String::new(),
                },
                CanonicalSource {
                    stream_id: 3,
                    name: "RAW".into(),
                    score: 40,
                    logo: None,
                    tv_archive: false,
                    tv_archive_duration: None,
                    direct_source: None,
                    origin_host: String::new(),
                },
            ],
        };
        let picked = ch.pick_archive_source().unwrap();
        // RAW is highest-scored but doesn't support archive; pick 4K instead.
        assert_eq!(picked.stream_id, 2);
    }

    #[test]
    fn pick_archive_source_returns_none_when_no_archive() {
        use crate::canonical::CanonicalSource;
        use crate::xtream::ChannelKind;
        let ch = CanonicalChannel {
            key: "x".into(),
            name: "X".into(),
            kind: ChannelKind::Tv,
            sources: vec![CanonicalSource {
                stream_id: 1,
                name: "RAW".into(),
                score: 40,
                logo: None,
                tv_archive: false,
                tv_archive_duration: None,
                direct_source: None,
                origin_host: String::new(),
            }],
        };
        assert!(ch.pick_archive_source().is_none());
    }

    #[test]
    fn segment_token_marks_probe_requests_non_mutating() {
        let url = proxy_url(
            "https://iptv.example.test",
            "https://upstream.example/chunklist.m3u8",
            "antena1",
            "https://upstream.example/master.m3u8",
            false,
            None,
            None,
        );
        let token = url.rsplit('/').next().unwrap();
        let decoded = decode_segment_token(token).unwrap();
        assert!(decoded.probe);
        assert_eq!(decoded.u, "https://upstream.example/chunklist.m3u8");
        assert_eq!(
            decoded.p.as_deref(),
            Some("https://upstream.example/master.m3u8")
        );
        assert_eq!(decoded.c.as_deref(), Some("antena1"));
    }

    #[test]
    fn segment_token_tracks_failures_for_regular_playback() {
        let url = proxy_url(
            "https://iptv.example.test",
            "https://upstream.example/seg.ts",
            "rtp1",
            "https://upstream.example/live.m3u8",
            true,
            Some(5.0),
            Some("http://cf.example"),
        );
        let token = url.rsplit('/').next().unwrap();
        let decoded = decode_segment_token(token).unwrap();
        assert!(!decoded.probe);
        assert_eq!(decoded.u, "https://upstream.example/seg.ts");
        assert_eq!(decoded.d, Some(5.0));
        assert_eq!(decoded.h.as_deref(), Some("http://cf.example"));
    }
}
