use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};

use crate::api::request_base_url;
use axum::response::Response;
use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::{debug, info, warn};
use url::Url;

use crate::blacklist::FailureKind;
use crate::canonical::CanonicalChannel;
use crate::codec::{classify_ts_chunk, strip_subtitle_pids};
use crate::measured::MeasuredStore;
use crate::play_log::{AttemptOutcome, PlayAttempt, PlayEvent};
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
pub(crate) struct Candidate {
    pub(crate) url: String,
    pub(crate) host: String,
    /// Upstream stream_id. For Xtream sources, parsed from the URL pattern.
    /// For radio (`direct_source`), copied from `CanonicalSource.stream_id`
    /// (a synthetic high-bit-set value); radio URLs don't carry it in their
    /// path, so propagation through `Candidate` is the only way the
    /// measurement layer can key by `(stream_id, host)`.
    pub(crate) stream_id: u64,
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
    /// True iff the requesting client advertised the `dvb_safe` cap (via
    /// `&caps=...,dvb_safe,...` on the play URL). When true, the DVB
    /// subtitle PIDs ride through verbatim — the client demuxer handles
    /// them. When false (legacy / non-dvb_safe clients), `handle_ts_segment`
    /// strips the PIDs as before. Defaults to false on legacy tokens so
    /// existing clients keep getting the stripped stream.
    #[serde(default, rename = "s", skip_serializing_if = "is_false")]
    dvb_safe: bool,
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
    /// Comma-list of client capability tags (e.g. `hls,h264,aac,dvb_safe`).
    /// The play-URL is the only place the proxy reliably sees per-request
    /// caps because the playback path bypasses our XHR wrapper that sets
    /// `X-Client-Caps` — webOS uses `<video src>` directly and hls.js's
    /// `loadSource` ships without an `xhrSetup` hook. Parsed once in
    /// `play_playlist` and baked into each segment's `SegmentToken` so
    /// the DVB-strip decision (Step 4 §9) is consistent for the play
    /// session. Absent on legacy clients → strip applies, matching the
    /// pre-Step-4 default.
    #[serde(default)]
    pub caps: Option<String>,
    /// Step 9 user override: base64-url-no-pad encoded upstream URL the
    /// client wants tried first. Validated against the current
    /// `build_candidates` output and rejected with 404 when not present
    /// (security — don't let a hostile client proxy arbitrary URLs).
    /// Promoted to position 0 for this play only; no state mutation.
    #[serde(default)]
    pub force_url: Option<String>,
    /// Phase 3 probe pin: when `probe=1` AND `probe_stream_id=<N>` are both
    /// present, restrict the play-loop candidate list to candidates whose
    /// upstream stream_id matches N. If every alive host for that variant
    /// fails, return 502 + `x-fail-reason: probe-pin-failed` so the client
    /// drops the relevant cap. Ignored on non-probe requests (safety: a
    /// real play must never accidentally pin to a single variant).
    #[serde(default)]
    pub probe_stream_id: Option<u64>,
}

/// Parse the `caps=` comma-list and return whether `dvb_safe` is present.
/// Lowercased for safety; legacy clients with no `caps=` → `false`, which
/// is the pre-Step-4 strip-by-default behaviour.
fn client_has_cap(caps: &Option<String>, tag: &str) -> bool {
    let Some(raw) = caps.as_deref() else { return false };
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .any(|c| c == tag)
}

/// Parse the `caps=` comma-list into a deduplicated vector of lowercased
/// tags. None / empty → None (caller treats this as "no filter") so legacy
/// clients without a `caps=` param keep working under both v1 and v2.
fn parse_caps_list(caps: &Option<String>) -> Option<Vec<String>> {
    let raw = caps.as_deref()?;
    let mut out: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub async fn play_playlist(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(params): Query<PlayParams>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    // Trim known route suffixes. `.audio` is the non-HLS radio route emitted
    // by `api::list_channels` for Mp3/Aac/Icecast/Playlist sources; it falls
    // through to `play_audio` once the channel is looked up.
    let key = name
        .trim_end_matches(".m3u8")
        .trim_end_matches(".ts")
        .trim_end_matches(".audio");
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

    // Build the candidate list + apply any `?force_url` BEFORE dispatching
    // by kind. The radio path also needs force_url honoured + the 404
    // semantics on malformed / non-candidate input — handling it here
    // avoids duplicating the validation in `play_audio`.
    //
    // Phase 3: parse client caps from `?caps=...,...`. Probe requests pass
    // `None` so the candidate filter doesn't reject the probe target on
    // grounds of "the client probably doesn't support this cap" — the
    // probe is the thing finding out whether the client supports it.
    let client_caps_list: Option<Vec<String>> = if probe_request {
        None
    } else {
        parse_caps_list(&params.caps)
    };
    let mut candidates = build_candidates(
        &state,
        &channel,
        client_caps_list.as_deref(),
    );
    if candidates.is_empty() {
        if !probe_request && client_caps_list.is_some() {
            warn!(
                play = %play_id,
                channel = %channel.key,
                "caps-mismatch: no variant satisfies client caps"
            );
            let mut resp = Response::new(Body::from("no variant matches client caps"));
            *resp.status_mut() = StatusCode::BAD_GATEWAY;
            resp.headers_mut().insert(
                "x-fail-reason",
                HeaderValue::from_static("caps-mismatch"),
            );
            return Ok(resp);
        }
        warn!(play = %play_id, channel = %channel.key, "no candidate sources for channel");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no candidate sources for channel".into()));
    }
    // Phase 3 probe pin: restrict to the targeted variant. Promotion is
    // not enough — the play loop falls through to position 1 on transient
    // failures, which on the probe path could be a sibling lacking the
    // cap → false positive. Restriction makes the probe fail closed.
    if probe_request {
        if let Some(sid) = params.probe_stream_id {
            candidates.retain(|c| c.stream_id == sid);
            if candidates.is_empty() {
                let mut resp = Response::new(Body::from("probe pin matched no candidate"));
                *resp.status_mut() = StatusCode::BAD_GATEWAY;
                resp.headers_mut().insert(
                    "x-fail-reason",
                    HeaderValue::from_static("probe-pin-failed"),
                );
                return Ok(resp);
            }
        }
    }
    if let Some(encoded) = params.force_url.as_deref() {
        let forced = apply_force_url(&mut candidates, encoded)
            .map_err(|_| (StatusCode::NOT_FOUND, "unknown force_url".to_string()))?;
        info!(
            play = %play_id,
            channel = %channel.key,
            forced = %forced,
            "force_url honoured (promoted to position 0 for this play)"
        );
    }

    // Non-HLS radio dispatch. The new `.audio` route also lands here (we
    // accept `.m3u8` for backwards compatibility); we pick the path by the
    // channel's first source format, not by the URL extension, so a stale
    // client URL still routes correctly when the catalogue's format flips.
    if matches!(channel.kind, crate::xtream::ChannelKind::Radio) {
        let fmt = channel
            .sources
            .iter()
            .find_map(|s| s.radio_format)
            .unwrap_or(crate::radio::RadioFormat::Hls);
        if !matches!(fmt, crate::radio::RadioFormat::Hls) {
            return play_audio(state, channel, candidates, fmt, public_base, play_id, client_pid).await;
        }
    }

    // Parse the per-request caps once (§4 plumbing). Today only `dvb_safe`
    // affects proxy behaviour (per-request DVB-strip vs verbatim); the
    // full cap matrix is still client-side (`X-Client-Caps` on /api/*).
    // Probe requests don't claim caps — they're testing whether the
    // client can play, so they ride through with `dvb_safe = false`.
    let client_has_dvb_safe = !probe_request && client_has_cap(&params.caps, "dvb_safe");

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
                client_has_dvb_safe,
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
                    state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
                    state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
    // Phase 3: when a probe pinned a specific stream_id and every host
    // failed, surface that as `probe-pin-failed` so the client drops the
    // cap. Otherwise return the generic 502.
    if probe_request && params.probe_stream_id.is_some() {
        let mut resp = Response::new(Body::from(err_text));
        *resp.status_mut() = StatusCode::BAD_GATEWAY;
        resp.headers_mut().insert(
            "x-fail-reason",
            HeaderValue::from_static("probe-pin-failed"),
        );
        return Ok(resp);
    }
    Err((StatusCode::BAD_GATEWAY, err_text))
}

pub(crate) fn build_candidates(
    state: &AppState,
    channel: &CanonicalChannel,
    client_caps: Option<&[String]>,
) -> Vec<Candidate> {
    let alive = state.hosts.alive_hosts_ranked();
    let v2 = state.config.caps_v2_per_variant;
    let stale_secs = state.config.caps_v2_stale_secs;
    let now = time::OffsetDateTime::now_utc();
    let mut cands: Vec<Candidate> = Vec::new();

    // Per-source emission. No more host-bad / url-failed / demoted exclusion
    // here (plan §8 strict reading: "no source disappears because we
    // *suspect* it's broken"). Every (source × host) reachable for the
    // channel is enumerated; rank-tuple penalties (cool-off step, host
    // badness) sort the broken ones to the bottom without removing them.
    //
    // Under v2 (`caps_v2_per_variant=true`), this scope tightens to
    // `alive ∧ ¬blacklisted ∧ ¬stale` so the rank-winner the emit picks
    // matches what /play can actually rotate to.
    //
    // Emission order is preserved so the stable sort keeps primary hosts
    // ahead of speculative ones among rank-equal candidates: a primary that
    // tied with a speculative on rank still goes first.
    let host_eligible = |host: &str, stream_id: u64| -> bool {
        if !v2 {
            return true;
        }
        if host.is_empty() {
            return true;
        }
        // Shared v2 host-eligibility predicate so `channel_caps_v2`'s
        // rank-winner computation and this candidate list can't drift.
        crate::caps_cache::host_eligible_v2(
            &state.measured,
            &state.blacklist,
            &alive,
            host,
            stream_id,
            stale_secs,
            now,
        )
    };

    let caps_satisfied = |stream_id: u64| -> bool {
        let Some(caps) = client_caps else { return true; };
        // Under v2 we require the variant to have at least one alive host
        // in scope (else `variant_caps_required` returns None and the
        // variant is dropped entirely).
        let Some(req) = crate::caps_cache::variant_caps_required(
            &state.measured,
            &state.blacklist,
            stream_id,
            &alive,
            stale_secs,
            now,
        ) else {
            return false;
        };
        req.iter().all(|c| caps.iter().any(|x| x == c))
    };

    for src in &channel.sources {
        if v2 && !caps_satisfied(src.stream_id) {
            continue;
        }
        if let Some(direct) = &src.direct_source {
            // Radio: URL given verbatim by `direct_source` — one candidate
            // per source, no host fanout.
            let host = derive_host(direct).unwrap_or_default();
            cands.push(Candidate {
                url: direct.clone(),
                host,
                stream_id: src.stream_id,
            });
            continue;
        }
        // Xtream TV source: primary host first (the one that reported the
        // stream), then speculative fanout to every other alive host.
        let primary_host = src.origin_host.as_str();
        if !primary_host.is_empty() && host_eligible(primary_host, src.stream_id) {
            cands.push(make_xtream_candidate(state, primary_host, src.stream_id));
        } else if !primary_host.is_empty() && !v2 && alive.iter().any(|h| h == primary_host) {
            cands.push(make_xtream_candidate(state, primary_host, src.stream_id));
        } else if !primary_host.is_empty() && !v2 {
            // origin_host known but not currently alive — still keep the
            // candidate so cool-off / host-penalty rank it; we don't
            // hard-exclude based on liveness probes.
            cands.push(make_xtream_candidate(state, primary_host, src.stream_id));
        }
        for host in &alive {
            if host == primary_host {
                continue;
            }
            if !host_eligible(host, src.stream_id) {
                continue;
            }
            cands.push(make_xtream_candidate(state, host, src.stream_id));
        }
    }

    dedup_preserving_order(&mut cands);

    // Measurement-driven rank. Snapshot the play log once (not per-candidate)
    // so success_score is O(candidates × history) rather than O(candidates²).
    // Stable sort preserves the enumeration order for rank-equal candidates,
    // so primary-host candidates stay ahead of speculative ones at the same
    // tier of measurement/cool-off.
    let log_snap = state.play_log.snapshot();
    match channel.kind {
        crate::xtream::ChannelKind::Tv => {
            cands.sort_by(|a, b| {
                let ka = source_rank_key_tv(
                    &channel.key,
                    a.stream_id,
                    &a.url,
                    &a.host,
                    &state.measured,
                    &state.blacklist,
                    &log_snap,
                );
                let kb = source_rank_key_tv(
                    &channel.key,
                    b.stream_id,
                    &b.url,
                    &b.host,
                    &state.measured,
                    &state.blacklist,
                    &log_snap,
                );
                kb.cmp(&ka)
            });
        }
        crate::xtream::ChannelKind::Radio => {
            cands.sort_by(|a, b| {
                let ka = source_rank_key_radio(
                    &channel.key,
                    a.stream_id,
                    &a.url,
                    &a.host,
                    &state.measured,
                    &state.blacklist,
                    &log_snap,
                );
                let kb = source_rank_key_radio(
                    &channel.key,
                    b.stream_id,
                    &b.url,
                    &b.host,
                    &state.measured,
                    &state.blacklist,
                    &log_snap,
                );
                kb.cmp(&ka)
            });
        }
    }

    cands
}

// --- Rank-key helpers ------------------------------------------------------
//
// Lexicographic comparison key for sorting candidates. Bigger is better.
// Per `ChannelKind`: TV and Radio have different quality dimensions, and
// mixing them in a single tuple compares apples to oranges. `build_candidates`
// dispatches on `channel.kind` so each sort only sees one kind.
//
// Common prefix (both kinds):
//   0. -cool_off_penalty  — fresh URLs first; URLs in long cool-off sink.
//                            Plan §4: never excluded, but heavily demoted.
//   1. -host_penalty      — host-bad URLs get demoted in-tuple (2 = bad).
//   2. measured?          — measured beats unmeasured.
//   3. success_bucket     — history-aware reliability for (stream_id, host).
//   4. lkg_bonus          — Step 5: decayed by age (3/2/1/0 at <1h/<6h/<24h/older).
//
// TV-specific tail (slots 5..=10): HDR, bpp_bucket, pixels, codec_rank,
// fps_rank, raw_kbps — preserves the existing video quality ordering.
//
// Radio-specific tail (slots 5..=7): kbps_bucket, sample_rate_bucket,
// channels — Phase 8 fills these in from the ADTS extractor; for now they
// are placeholder zeros.

pub(crate) type TvRankKey = (
    i32, // -cool_off_penalty
    i32, // -host_penalty
    i32, // measured? (1/0)
    i32, // success_bucket
    i32, // lkg_bonus
    i32, // hdr_rank
    i32, // bpp_bucket
    i64, // pixels
    i32, // codec_rank
    i32, // fps_rank
    i32, // raw_kbps
);

/// Public opaque alias for caps_cache::channel_caps_v2 to compare
/// rank keys of candidate variants without leaking the tuple shape.
pub type TvRankKeyOpaque = TvRankKey;

/// Compute the TV rank key for a (stream_id, host) pair. Public so
/// `caps_cache::channel_caps_v2` can pick the rank-winner variant the
/// same way `build_candidates` does.
pub fn compute_tv_rank_key(
    channel_key: &str,
    stream_id: u64,
    url: &str,
    host: &str,
    measured: &MeasuredStore,
    blacklist: &crate::blacklist::Blacklist,
    log_snap: &[PlayEvent],
) -> TvRankKey {
    source_rank_key_tv(channel_key, stream_id, url, host, measured, blacklist, log_snap)
}

type RadioRankKey = (
    i32, // -cool_off_penalty
    i32, // -host_penalty
    i32, // measured? (1/0)
    i32, // success_bucket
    i32, // lkg_bonus
    i32, // kbps_bucket — Phase 8 (ADTS)
    i32, // sample_rate_bucket — Phase 8
    i32, // channels — Phase 8
);

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

/// `0` for healthy hosts, `2` for hosts the blacklist has flagged via
/// `is_host_bad`. Used by both per-kind rank-key helpers as the second
/// slot — host-bad URLs get demoted in-tuple instead of removed.
fn host_penalty(host: &str, bl: &crate::blacklist::Blacklist) -> i32 {
    if !host.is_empty() && bl.is_host_bad(host) { 2 } else { 0 }
}

/// LKG decay tiers (Step 5). Returns 3 / 2 / 1 / 0 depending on age. Used
/// as the fifth slot in the rank tuple — beats quality among rank-equal
/// siblings, but a measurement-better sibling still wins because the
/// quality dimensions come *after* lkg_bonus in the tuple.
fn lkg_bonus(channel_key: &str, url: &str, bl: &crate::blacklist::Blacklist) -> i32 {
    match bl.last_known_good_age(channel_key, url) {
        Some(age) if age < Duration::from_secs(3600) => 3,        // < 1 h
        Some(age) if age < Duration::from_secs(6 * 3600) => 2,    // < 6 h
        Some(age) if age < Duration::from_secs(24 * 3600) => 1,   // < 24 h
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn source_rank_key_tv(
    channel_key: &str,
    stream_id: u64,
    url: &str,
    host: &str,
    measured: &MeasuredStore,
    bl: &crate::blacklist::Blacklist,
    log_snap: &[PlayEvent],
) -> TvRankKey {
    let success = success_bucket(success_score(stream_id, host, log_snap));
    let cool_off = -bl.cool_off_penalty(url);
    let host_pen = -host_penalty(host, bl);
    let lkg = lkg_bonus(channel_key, url, bl);
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
                cool_off,
                host_pen,
                1,
                success,
                lkg,
                hdr,
                bpp_bucket(q.bitrate_kbps, q.width, q.height),
                (q.width as u64 * q.height as u64) as i64,
                codec_rank(q.codec.as_deref()),
                fps_rank(q.framerate),
                q.bitrate_kbps.unwrap_or(0) as i32,
            )
        }
        None => (cool_off, host_pen, 0, success, lkg, 0, 0, 0, 0, 0, 0),
    }
}

#[allow(clippy::too_many_arguments)]
fn source_rank_key_radio(
    channel_key: &str,
    stream_id: u64,
    url: &str,
    host: &str,
    measured: &MeasuredStore,
    bl: &crate::blacklist::Blacklist,
    log_snap: &[PlayEvent],
) -> RadioRankKey {
    let success = success_bucket(success_score(stream_id, host, log_snap));
    let cool_off = -bl.cool_off_penalty(url);
    let host_pen = -host_penalty(host, bl);
    let lkg = lkg_bonus(channel_key, url, bl);
    match measured.get(stream_id, host) {
        Some(q) => (
            cool_off,
            host_pen,
            1,
            success,
            lkg,
            audio_kbps_bucket(q.bitrate_kbps),
            audio_sample_rate_bucket(q.sample_rate_hz),
            audio_channels_bucket(q.audio_channels),
        ),
        None => (cool_off, host_pen, 0, success, lkg, 0, 0, 0),
    }
}

/// Plan §10 radio ADTS rank buckets.
fn audio_kbps_bucket(kbps: Option<u32>) -> i32 {
    match kbps {
        Some(k) if k >= 320 => 4,
        Some(k) if k >= 192 => 3,
        Some(k) if k >= 128 => 2,
        Some(k) if k >= 64 => 1,
        Some(_) => 0,
        None => 0,
    }
}

fn audio_sample_rate_bucket(hz: Option<u32>) -> i32 {
    match hz {
        Some(48000) => 3,
        Some(44100) => 2,
        Some(32000) => 1,
        _ => 0,
    }
}

fn audio_channels_bucket(channels: Option<u8>) -> i32 {
    match channels {
        Some(2) => 2,
        Some(1) => 1,
        _ => 0,
    }
}

/// Validate `?force_url=<b64>` and promote the matching candidate to
/// position 0 in `candidates`. Returns the chosen URL on success, or
/// `Err(())` on any failure (malformed base64, non-UTF8, URL not in the
/// current candidate set). The caller maps Err → 404 — security: don't
/// let a hostile client coax the proxy into fetching arbitrary URLs.
fn apply_force_url(candidates: &mut Vec<Candidate>, encoded: &str) -> Result<String, ()> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .map_err(|_| ())?;
    let url = std::str::from_utf8(&raw).map_err(|_| ())?.to_string();
    let pos = candidates.iter().position(|c| c.url == url).ok_or(())?;
    if pos != 0 {
        let cand = candidates.remove(pos);
        candidates.insert(0, cand);
    }
    Ok(url)
}

fn make_xtream_candidate(state: &AppState, host: &str, stream_id: u64) -> Candidate {
    let url = state.xtream.stream_url(host, stream_id, "m3u8");
    Candidate {
        url,
        host: host.to_string(),
        stream_id,
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
    client_has_dvb_safe: bool,
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
            RewriteCtx {
                channel_key: &channel.key,
                source_url: &cand.url,
                track_failures,
                upstream_host: Some(&cand.host),
                client_has_dvb_safe,
            },
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

/// Non-HLS radio playback: stream raw audio bytes from a working upstream.
/// Mirrors `play_playlist`'s candidate-iteration + per-attempt-timeout +
/// blacklist/play_log accounting, but the success criterion is "got a 2xx
/// response with an audio content-type" rather than "parseable EXTM3U body".
/// On success, the response body is the upstream's bytes_stream — the client
/// element (`<video src>` on webOS / desktop) decodes it natively.
async fn play_audio(
    state: Arc<AppState>,
    channel: CanonicalChannel,
    // Pre-built candidate list (with any `?force_url` already applied by
    // `play_playlist`). Hoisted out of here so the force_url validation
    // path covers radio too — see Phase 9 R2 issue 2.
    candidates: Vec<Candidate>,
    fmt: crate::radio::RadioFormat,
    _public_base: String,
    play_id: String,
    client_pid: Option<String>,
) -> Result<Response, (StatusCode, String)> {
    let _active_play_guard = ActivePlayGuard::new(&state.active_plays);
    if candidates.is_empty() {
        warn!(play = %play_id, channel = %channel.key, "no candidate sources for radio");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no candidate sources for channel".into()));
    }

    let per_attempt = Duration::from_secs(state.config.proxy.per_attempt_timeout_secs);
    let budget = Duration::from_secs(state.config.proxy.play_budget_secs);
    let started = Instant::now();
    let started_wall = OffsetDateTime::now_utc();
    let mut attempts: Vec<PlayAttempt> = Vec::new();
    let mut last_err: Option<String> = None;
    let mut tried = 0usize;

    info!(
        play = %play_id,
        channel = %channel.key,
        candidates = candidates.len(),
        format = ?fmt,
        "audio play start",
    );

    for (idx, cand) in candidates.iter().enumerate() {
        let _ = idx;
        let elapsed = started.elapsed();
        if elapsed >= budget {
            warn!(play = %play_id, channel = %channel.key, "audio play budget exhausted");
            break;
        }
        let attempt_timeout = per_attempt.min(budget.saturating_sub(elapsed));
        tried += 1;
        let attempt_start = Instant::now();
        match tokio::time::timeout(
            attempt_timeout,
            attach_audio_upstream(&state, &cand.url, fmt),
        )
        .await
        {
            Ok(Ok(resp)) => {
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                state.blacklist.note_url_succeeded(&channel.key, &cand.url);
                if let Some(pid) = client_pid.as_deref() {
                    state.play_sessions.note(pid, &channel.key, &cand.url);
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
                    "audio play ok",
                );
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
                return Ok(stream_audio_response(resp, &cand.url));
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
                    "audio upstream failed",
                );
                state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
                    "audio upstream timed out",
                );
                state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
        "all {tried}/{total} audio candidates failed for {channel} (last: {err})",
        total = candidates.len(),
        channel = channel.key,
        err = last_err.as_deref().unwrap_or("budget exhausted"),
    );
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
    Err((StatusCode::BAD_GATEWAY, err_text))
}

async fn attach_audio_upstream(
    state: &AppState,
    url: &str,
    fmt: crate::radio::RadioFormat,
) -> anyhow::Result<reqwest::Response> {
    let final_url = if matches!(fmt, crate::radio::RadioFormat::Playlist) {
        resolve_playlist_indirection(state, url).await?
    } else {
        url.to_string()
    };
    let resp = state
        .upstream_http
        .get(&final_url)
        // Tell Icecast/Shoutcast we want raw audio, not metadata. Without this,
        // some servers return audio interleaved with `StreamTitle='...'` blocks
        // every N bytes that the browser/webOS demuxer can't handle.
        .header("Icy-MetaData", "0")
        .header(reqwest::header::ACCEPT, "audio/*, */*;q=0.5")
        .send()
        .await?
        .error_for_status()?;
    let landed = resp.url().clone();
    if is_abuse_url(&landed) {
        anyhow::bail!("upstream redirected to abuse page: {}", landed);
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    // Accept anything plausibly audio. Some Shoutcast servers return empty or
    // `application/octet-stream`; trust the RadioFormat hint there.
    let looks_audio = ct.starts_with("audio/")
        || ct.contains("mpeg")
        || ct.contains("aacp")
        || ct.contains("aac")
        || ct == "application/ogg"
        || ct.is_empty()
        || ct == "application/octet-stream";
    if !looks_audio {
        anyhow::bail!("non-audio content-type: {ct}");
    }
    Ok(resp)
}

/// Resolve a `.pls`/`.m3u` indirection to the underlying audio URL. Cached
/// per source URL for 1 hour — the indirection rarely changes, and resolving
/// on every play attempt would double the upstream load for those entries.
async fn resolve_playlist_indirection(state: &AppState, url: &str) -> anyhow::Result<String> {
    const TTL: Duration = Duration::from_secs(3600);
    if let Some(entry) = state.playlist_resolver_cache.get(url) {
        let (cached, at) = entry.value();
        if at.elapsed() < TTL {
            return Ok(cached.clone());
        }
    }
    let resp = tokio::time::timeout(
        Duration::from_secs(state.config.proxy.per_attempt_timeout_secs),
        state.upstream_http.get(url).send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out fetching playlist indirection"))??
    .error_for_status()?;
    let body = resp.text().await?;
    let resolved = parse_pls_or_m3u(&body)
        .ok_or_else(|| anyhow::anyhow!("no usable audio URL in .pls/.m3u body"))?;
    state
        .playlist_resolver_cache
        .insert(url.to_string(), (resolved.clone(), Instant::now()));
    Ok(resolved)
}

/// Parse the first usable audio URL out of a `.pls` / `.m3u` body. `.pls`
/// gives `^File\d+=<url>$`; non-HLS `.m3u` is a list of bare URLs, one per
/// line, optionally with `#`-comments. We return the first `http(s)://...`
/// match. Caller validates by attempting the audio fetch.
fn parse_pls_or_m3u(body: &str) -> Option<String> {
    static PLS_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)^file\d+\s*=\s*(.+)$").unwrap());
    for line in body.lines() {
        let t = line.trim();
        if let Some(c) = PLS_RE.captures(t) {
            let u = c.get(1)?.as_str().trim().to_string();
            if u.starts_with("http") {
                return Some(u);
            }
        }
    }
    for line in body.lines() {
        let t = line.trim();
        if !t.is_empty() && !t.starts_with('#') && t.starts_with("http") {
            return Some(t.to_string());
        }
    }
    None
}

fn stream_audio_response(resp: reqwest::Response, upstream_url: &str) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let ct_value = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(HeaderValue::try_from)
        .and_then(Result::ok)
        .unwrap_or_else(|| HeaderValue::from_static("audio/mpeg"));
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, ct_value)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap();
    if let Ok(v) = HeaderValue::from_str(upstream_url) {
        response.headers_mut().insert("x-upstream", v);
    }
    response
}

/// Caps + bookkeeping flags carried through the playlist-rewrite pipeline
/// into each segment's `SegmentToken`. Bundles the per-request-flags fields
/// so we don't have to thread N extra bool params through `rewrite_playlist`
/// / `rewrite_tag_with_uri` / `proxy_url`.
#[derive(Debug, Clone, Copy)]
struct RewriteCtx<'a> {
    channel_key: &'a str,
    source_url: &'a str,
    track_failures: bool,
    upstream_host: Option<&'a str>,
    /// Plumbing for §9: did the client request `caps=...,dvb_safe,...` on
    /// the play URL? If so, segments ride through verbatim; otherwise
    /// `handle_ts_segment` strips DVB-subtitle PIDs as before.
    client_has_dvb_safe: bool,
}

fn rewrite_playlist(
    body: &str,
    playlist_url: &Url,
    public_base: &str,
    ctx: RewriteCtx<'_>,
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
            if let Some(rewritten) = rewrite_tag_with_uri(trimmed, &base, public_base, &ctx) {
                out.push_str(&rewritten);
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if let Ok(resolved) = base.join(trimmed) {
            out.push_str(&proxy_url(public_base, resolved.as_str(), pending_duration, &ctx));
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
    ctx: &RewriteCtx<'_>,
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
    let new_uri = proxy_url(public_base, resolved.as_str(), None, ctx);
    let mut s = String::with_capacity(line.len() + new_uri.len());
    s.push_str(&line[..start]);
    s.push_str(&new_uri);
    s.push_str(&line[start + rel_end..]);
    Some(s)
}

fn proxy_url(
    public_base: &str,
    absolute_upstream: &str,
    duration: Option<f32>,
    ctx: &RewriteCtx<'_>,
) -> String {
    let payload = SegmentToken {
        u: absolute_upstream.to_string(),
        p: Some(ctx.source_url.to_string()),
        c: Some(ctx.channel_key.to_string()),
        probe: !ctx.track_failures,
        d: duration,
        h: ctx.upstream_host.map(|s| s.to_string()),
        dvb_safe: ctx.client_has_dvb_safe,
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
        dvb_safe: false,
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
            RewriteCtx {
                channel_key: &channel_key,
                source_url: &source_url,
                track_failures: !segment.probe,
                upstream_host: segment.h.as_deref(),
                // Nested playlist inherits the outer segment's caps decision —
                // the master-level rewrite already baked dvb_safe in based on
                // the original play URL, so nested children get the same.
                client_has_dvb_safe: segment.dvb_safe,
            },
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
    if let (true, Some(sid)) = (is_ts, stream_id) {
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
            if let (Some(d), Some(h)) = (segment.d, segment.h.as_deref()) {
                if d > 0.0 && !bytes.is_empty() {
                    let kbps = bytes.len() as f64 * 8.0 / 1000.0 / d as f64;
                    state.per_play.note_segment_kbps(sid, h, kbps as f32);
                }
            }
        }
        let processed = handle_ts_segment(&state, sid, &bytes, &segment);
        // After handle_ts_segment has run (and possibly cached the
        // classification), push per-play metadata into the accumulator.
        // Idempotent — subsequent calls just refresh last_activity.
        if !segment.probe {
            if let Some(h) = segment.h.as_deref() {
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
                        Some(c.dvb_unsafe),
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

    // Phase 8 (Step 10) audio branch: AAC radio segments. Classify the
    // ADTS header for sample_rate/channels, compute kbps from the byte
    // count + EXTINF duration, and feed both into the per-play accumulator.
    // We use the segment_id from the token (radio sources synth a
    // high-bit-set stream_id at canonical-build time).
    let is_aac = content_type.contains("aac")
        || upstream_path.ends_with(".aac")
        || upstream_path.ends_with(".m4a");
    if let (true, Some(sid)) = (is_aac, segment.p.as_deref().and_then(stream_id_from_source_url).or(stream_id)) {
        let upstream_headers = resp.headers().clone();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("audio segment body read failed: {} → {}", upstream, e);
                mark_segment_failure(&state, &segment);
                return Err((StatusCode::BAD_GATEWAY, format!("body read: {e}")));
            }
        };
        if !segment.probe {
            if let Some(h) = segment.h.as_deref() {
                let duration = segment.d.unwrap_or(0.0) as f64;
                if let Some(audio_cls) = crate::codec::classify_aac_chunk(&bytes, duration) {
                    state.per_play.note_audio_classification(
                        sid,
                        h,
                        audio_cls.sample_rate_hz,
                        audio_cls.audio_channels,
                    );
                    // Feed kbps via the EWMA so multi-segment averaging happens
                    // the same way as for TV bitrate.
                    if let Some(k) = audio_cls.kbps {
                        state.per_play.note_segment_kbps(sid, h, k as f32);
                    }
                }
            }
        }
        return passthrough_response(status, &upstream_headers, bytes);
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
    // Per-request DVB-strip decision (Step 4 + architecture.md §9).
    //   - Client claimed `dvb_safe` cap on the play URL → verbatim
    //     passthrough; the client demuxer can handle the subs.
    //   - Client didn't claim it (legacy default) → strip the subtitle
    //     PIDs that are strippable; pass through verbatim if the PCR
    //     collision case (`dvb_unsafe`) makes stripping impossible.
    //
    // The per-source demote that lived here before is gone: Step 7's
    // `caps_required` derivation (which uses Sample.dvb_unsafe across all
    // a channel's sources) is the right place to add the `dvb_safe` cap
    // requirement, instead of demoting individual sources at segment time.
    if segment.dvb_safe {
        return bytes.to_vec();
    }
    let pids = classification.strippable_subtitle_pids();
    let Some(pmt_pid) = classification.pmt_pid else {
        return bytes.to_vec();
    };
    if pids.is_empty() {
        // Either no subs at all, or PCR-colliding subs (unstrippable). Pass
        // through verbatim — Step 7's caps_required is what gates the
        // unstrippable case at the channel-list level.
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
    // Both the segment URL itself and its parent playlist URL get a
    // ServerSide bump (upstream produced an error / abuse redirect / etc.).
    // No more `drop_last_known_good` — Step 5 turned LKG into a decayed
    // rank-tuple bonus that naturally fades when the URL stops getting
    // marked good, so a manual drop isn't needed.
    state.blacklist.note_failure(&segment.u, FailureKind::ServerSide);
    if let Some(source_url) = segment.p.as_deref() {
        state.blacklist.note_failure(source_url, FailureKind::ServerSide);
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

/// Pick up to `count` siblings (excluding the one at `served_idx`) for
/// background validation. R2 issue 4: cool-off / blacklist state is
/// **not** consulted here — validating cooled URLs is the only way they
/// can recover via `note_url_succeeded`. Filtering them out would lock
/// them in cool-off forever.
fn pick_validation_candidates(
    candidates: &[Candidate],
    served_idx: usize,
    count: usize,
) -> Vec<Candidate> {
    candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != served_idx)
        .map(|(_, c)| c.clone())
        .take(count)
        .collect()
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
    let to_validate = pick_validation_candidates(candidates, served_idx, count);
    if to_validate.is_empty() {
        return;
    }
    let timeout = Duration::from_secs(state.config.proxy.opportunistic_validate_timeout_secs);
    tokio::spawn(async move {
        // R2 issue 4 / plan §4 line 134: opportunistic validation is NOT on
        // the carve-out list (only EPG, probe redirect, catchup keep their
        // is_url_failed exclusion). Validating cooled URLs lets a recovered
        // host walk back out of cool-off via `note_url_succeeded` here;
        // skipping them would leave them stuck cooling indefinitely.
        for cand in to_validate {
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
                        state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
                        continue;
                    }
                    if is_abuse_url(&final_url) {
                        warn!(
                            channel = %channel_key,
                            url = %cand.url,
                            "opportunistic validation: abuse redirect"
                        );
                        state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
                                state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
                                        state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
                                    }
                                }
                            } else {
                                warn!(
                                    channel = %channel_key,
                                    url = %cand.url,
                                    "opportunistic validation: not a playlist"
                                );
                                state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
                            }
                        }
                        Err(e) => {
                            warn!(
                                channel = %channel_key,
                                url = %cand.url,
                                err = %e,
                                "opportunistic validation: body read failed"
                            );
                            state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
                    state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
                }
                Err(_) => {
                    warn!(
                        channel = %channel_key,
                        url = %cand.url,
                        "opportunistic validation: timeout"
                    );
                    state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
                }
            }
        }
    });
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
            // Catchup doesn't honour client caps for now — the archive
            // pipeline pre-dates the per-request dvb_safe plumbing and the
            // catchup catalog is narrow enough that always-strip is safe.
            // If a catchup channel turns out to need verbatim DVB later,
            // thread caps through `catchup_play` then.
            fetch_and_rewrite_playlist(&state, &channel, &cand, public_base, true, false),
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
                state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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
                state.blacklist.note_failure(&cand.url, FailureKind::ServerSide);
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

    #[test]
    fn parse_pls_extracts_first_file_entry() {
        let body = "[playlist]\nNumberOfEntries=1\nFile1=http://stream.example/audio.mp3\nLength1=-1\nVersion=2\n";
        assert_eq!(
            parse_pls_or_m3u(body).as_deref(),
            Some("http://stream.example/audio.mp3"),
        );
    }

    #[test]
    fn parse_m3u_extracts_first_http_url() {
        let body = "#EXTM3U\n# comment\nhttp://stream.example/audio.aac\nhttp://second/url\n";
        assert_eq!(
            parse_pls_or_m3u(body).as_deref(),
            Some("http://stream.example/audio.aac"),
        );
    }

    #[test]
    fn parse_pls_returns_none_on_garbage() {
        assert!(parse_pls_or_m3u("hello world\n").is_none());
    }

    fn p(at: Option<&str>, from: Option<&str>, dur: Option<&str>) -> PlayParams {
        PlayParams {
            at: at.map(str::to_string),
            from: from.map(str::to_string),
            duration: dur.map(str::to_string),
            probe: false,
            pid: None,
            caps: None,
            force_url: None,
            probe_stream_id: None,
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
                    radio_format: None,
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
                    radio_format: None,
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
                    radio_format: None,
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
                radio_format: None,
            }],
        };
        assert!(ch.pick_archive_source().is_none());
    }

    fn ctx<'a>(
        ch: &'a str,
        src: &'a str,
        host: Option<&'a str>,
        track: bool,
        dvb_safe: bool,
    ) -> RewriteCtx<'a> {
        RewriteCtx {
            channel_key: ch,
            source_url: src,
            track_failures: track,
            upstream_host: host,
            client_has_dvb_safe: dvb_safe,
        }
    }

    #[test]
    fn segment_token_marks_probe_requests_non_mutating() {
        let url = proxy_url(
            "https://iptv.example.test",
            "https://upstream.example/chunklist.m3u8",
            None,
            &ctx("antena1", "https://upstream.example/master.m3u8", None, false, false),
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
        assert!(!decoded.dvb_safe, "probes don't claim dvb_safe");
    }

    #[test]
    fn segment_token_tracks_failures_for_regular_playback() {
        let url = proxy_url(
            "https://iptv.example.test",
            "https://upstream.example/seg.ts",
            Some(5.0),
            &ctx("rtp1", "https://upstream.example/live.m3u8", Some("http://cf.example"), true, false),
        );
        let token = url.rsplit('/').next().unwrap();
        let decoded = decode_segment_token(token).unwrap();
        assert!(!decoded.probe);
        assert_eq!(decoded.u, "https://upstream.example/seg.ts");
        assert_eq!(decoded.d, Some(5.0));
        assert_eq!(decoded.h.as_deref(), Some("http://cf.example"));
        assert!(!decoded.dvb_safe);
    }

    #[test]
    fn segment_token_round_trip_includes_dvb_safe() {
        // dvb_safe = true → token carries the bit and the segment handler
        // will pass bytes verbatim instead of stripping DVB-subtitle PIDs.
        let url = proxy_url(
            "https://iptv.example.test",
            "https://upstream.example/seg.ts",
            Some(5.0),
            &ctx("rtp1", "https://upstream.example/live.m3u8", Some("http://cf.example"), true, true),
        );
        let token = url.rsplit('/').next().unwrap();
        let decoded = decode_segment_token(token).unwrap();
        assert!(decoded.dvb_safe);
    }

    #[test]
    fn legacy_segment_token_decodes_to_dvb_safe_false() {
        // Pre-Phase-4 tokens were a bare upstream URL (not JSON). The
        // decode fallback synthesises an empty SegmentToken — `dvb_safe`
        // must default to `false` so legacy URLs still get the PID-strip
        // applied (matches the pre-Phase-4 behaviour).
        let legacy = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(b"https://upstream.example/seg.ts");
        let decoded = decode_segment_token(&legacy).unwrap();
        assert_eq!(decoded.u, "https://upstream.example/seg.ts");
        assert!(!decoded.dvb_safe);
        assert!(!decoded.probe);
    }

    fn empty_play_log() -> Vec<PlayEvent> {
        Vec::new()
    }

    fn fresh_blacklist() -> crate::blacklist::Blacklist {
        crate::blacklist::Blacklist::new(crate::config::BlacklistConfig {
            host_fail_threshold: 4,
            host_ttl_secs: 3600,
            cool_off_steps_secs: [60, 300, 1800, 21600],
            heartbeat_window_secs: 60,
            clean_play_reset_secs: 300,
        })
    }

    fn empty_measured() -> MeasuredStore {
        MeasuredStore::load_or_empty(std::path::PathBuf::from("/nonexistent-for-test.json"))
    }

    #[test]
    fn tv_rank_key_cool_off_dominates_quality() {
        // Two candidates: A is unmeasured but fresh (no cool-off); B is
        // measured-1080p but in cool-off step 3. Plan §4: cool-off
        // dominates — A wins.
        let bl = fresh_blacklist();
        let m = empty_measured();
        let log: Vec<PlayEvent> = empty_play_log();
        // Inject a cool-off step 3 on B's URL.
        for _ in 0..3 {
            bl.note_failure("http://cool/live/u/p/2.m3u8", crate::blacklist::FailureKind::ServerSide);
        }
        let key_a = source_rank_key_tv("rtp1", 1, "http://fresh/live/u/p/1.m3u8", "http://fresh", &m, &bl, &log);
        let key_b = source_rank_key_tv("rtp1", 2, "http://cool/live/u/p/2.m3u8", "http://cool", &m, &bl, &log);
        assert!(key_a > key_b, "fresh unmeasured beats cool-off-3 measured");
    }

    #[test]
    fn tv_rank_key_lkg_bonus_breaks_quality_tie() {
        // Two unmeasured candidates with the same cool-off. The LKG one
        // wins via the lkg_bonus slot.
        let bl = fresh_blacklist();
        bl.note_url_succeeded("rtp1", "http://lkg/live/u/p/1.m3u8");
        let m = empty_measured();
        let log: Vec<PlayEvent> = empty_play_log();
        let lkg_key = source_rank_key_tv("rtp1", 1, "http://lkg/live/u/p/1.m3u8", "http://lkg", &m, &bl, &log);
        let other_key = source_rank_key_tv("rtp1", 2, "http://other/live/u/p/2.m3u8", "http://other", &m, &bl, &log);
        assert!(lkg_key > other_key);
    }

    #[test]
    fn lkg_bonus_decays_with_age() {
        // < 1h → 3, < 6h → 2, < 24h → 1, older → 0. Test the boundaries
        // via direct call (function takes the blacklist, not a clock).
        let bl = fresh_blacklist();
        bl.note_url_succeeded("rtp1", "http://a");
        let now_bonus = lkg_bonus("rtp1", "http://a", &bl);
        assert_eq!(now_bonus, 3, "fresh LKG → tier 3");
        // Different URL → no bonus (we don't promote arbitrary siblings).
        assert_eq!(lkg_bonus("rtp1", "http://b", &bl), 0);
        // Unknown channel → no bonus.
        assert_eq!(lkg_bonus("missing", "http://a", &bl), 0);
    }

    fn mock_candidates() -> Vec<Candidate> {
        vec![
            Candidate { url: "http://a/live/u/p/1.m3u8".into(), host: "http://a".into(), stream_id: 1 },
            Candidate { url: "http://b/live/u/p/1.m3u8".into(), host: "http://b".into(), stream_id: 1 },
            Candidate { url: "http://c/live/u/p/1.m3u8".into(), host: "http://c".into(), stream_id: 1 },
        ]
    }

    fn b64url(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    #[test]
    fn force_url_promotes_matching_candidate_to_pos0() {
        let mut cands = mock_candidates();
        let url = "http://c/live/u/p/1.m3u8";
        let chosen = apply_force_url(&mut cands, &b64url(url)).expect("promote ok");
        assert_eq!(chosen, url);
        assert_eq!(cands[0].url, url);
        // Order otherwise preserved (a stays ahead of b after c moves up).
        assert_eq!(cands[1].url, "http://a/live/u/p/1.m3u8");
        assert_eq!(cands[2].url, "http://b/live/u/p/1.m3u8");
    }

    #[test]
    fn force_url_keeps_pos0_when_already_first() {
        let mut cands = mock_candidates();
        let url = cands[0].url.clone();
        apply_force_url(&mut cands, &b64url(&url)).expect("ok");
        assert_eq!(cands[0].url, url);
    }

    #[test]
    fn force_url_rejects_when_url_not_in_set() {
        let mut cands = mock_candidates();
        let err = apply_force_url(&mut cands, &b64url("http://intruder/live/u/p/99.m3u8"));
        assert!(err.is_err(), "URL not in current set must be rejected (404)");
        // Order untouched.
        assert_eq!(cands[0].url, "http://a/live/u/p/1.m3u8");
    }

    #[test]
    fn force_url_rejects_malformed_base64() {
        let mut cands = mock_candidates();
        assert!(apply_force_url(&mut cands, "@@@not-base64@@@").is_err());
    }

    #[test]
    fn force_url_rejects_non_utf8_payload() {
        let mut cands = mock_candidates();
        let bad = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xff, 0xfe, 0xfd]);
        assert!(apply_force_url(&mut cands, &bad).is_err());
    }

    #[test]
    fn opportunistic_validation_picks_siblings_without_consulting_cool_off() {
        // R2 issue 4: cool-off-aware filtering is removed inside the
        // schedule_opportunistic_validation pipeline. The pure selector
        // just walks index-by-index, skipping `served_idx`. A cooled
        // URL at the bottom of the rank ends up in the validation set
        // exactly because the filter is gone.
        let cands = vec![
            Candidate { url: "http://fresh-served/a.m3u8".into(), host: "http://fresh-served".into(), stream_id: 1 },
            Candidate { url: "http://fresh-sib/a.m3u8".into(), host: "http://fresh-sib".into(), stream_id: 1 },
            Candidate { url: "http://cool-sib/a.m3u8".into(), host: "http://cool-sib".into(), stream_id: 1 },
        ];
        let picked = pick_validation_candidates(&cands, 0, 2);
        assert_eq!(picked.len(), 2);
        // Order preserved; served_idx 0 skipped.
        assert_eq!(picked[0].url, "http://fresh-sib/a.m3u8");
        assert_eq!(picked[1].url, "http://cool-sib/a.m3u8");
    }

    #[test]
    fn opportunistic_validation_take_cap_respected() {
        let cands: Vec<Candidate> = (0..5)
            .map(|i| Candidate {
                url: format!("http://h{i}/a.m3u8"),
                host: format!("http://h{i}"),
                stream_id: 1,
            })
            .collect();
        let picked = pick_validation_candidates(&cands, 2, 2);
        assert_eq!(picked.len(), 2);
        assert!(!picked.iter().any(|c| c.host == "http://h2"));
    }

    #[test]
    fn force_url_works_against_radio_direct_source_candidates() {
        // R2 issue 2: `apply_force_url` is now called BEFORE the radio
        // dispatch in `play_playlist`, so direct_source radio URLs are
        // also subject to the 404 + promote semantics. Validate by feeding
        // a candidate list that mirrors what `build_candidates` produces
        // for radio (each direct_source becomes one Candidate, no host
        // fanout).
        let mut cands = vec![
            Candidate { url: "http://radio-a/aac".into(), host: "http://radio-a".into(), stream_id: 100 },
            Candidate { url: "http://radio-b/aac".into(), host: "http://radio-b".into(), stream_id: 101 },
        ];
        let chosen = apply_force_url(&mut cands, &b64url("http://radio-b/aac")).expect("promote ok");
        assert_eq!(chosen, "http://radio-b/aac");
        assert_eq!(cands[0].url, "http://radio-b/aac");
        // Non-candidate radio URL → still rejected.
        assert!(apply_force_url(&mut cands, &b64url("http://other-radio/aac")).is_err());
    }

    #[test]
    fn radio_rank_key_shape_matches_plan() {
        let bl = fresh_blacklist();
        let m = empty_measured();
        let log: Vec<PlayEvent> = empty_play_log();
        let k = source_rank_key_radio("antena1", 1, "http://r/a.m3u8", "http://r", &m, &bl, &log);
        // Unmeasured: cool=0, host=0, measured=0, success=5, lkg=0, audio 0/0/0
        assert_eq!(k.0, 0);
        assert_eq!(k.1, 0);
        assert_eq!(k.2, 0);
        assert_eq!(k.5, 0);
        assert_eq!(k.6, 0);
        assert_eq!(k.7, 0);
    }

    #[test]
    fn radio_rank_higher_kbps_wins_when_other_dims_equal() {
        // Phase 8: kbps_bucket beats lower bitrates among rank-equal
        // candidates that share success / LKG / etc.
        use crate::measured::{Sample, SampleSource};
        let bl = fresh_blacklist();
        let m = empty_measured();
        let log: Vec<PlayEvent> = empty_play_log();
        let mk = |kbps: u32| Sample {
            at: time::OffsetDateTime::now_utc(),
            source: SampleSource::Sweep,
            width: 0,
            height: 0,
            codec: Some("aac".into()),
            pix_fmt: None,
            color_transfer: None,
            framerate: None,
            bitrate_kbps: Some(kbps),
            dvb_unsafe: None,
            sample_rate_hz: Some(44100),
            audio_channels: Some(2),
            h264_excess_refs: None,
        };
        m.push(1, "http://lo", mk(96));
        m.push(2, "http://mid", mk(192));
        m.push(3, "http://hi", mk(320));
        let lo = source_rank_key_radio("ch", 1, "http://lo/a.m3u8", "http://lo", &m, &bl, &log);
        let mid = source_rank_key_radio("ch", 2, "http://mid/a.m3u8", "http://mid", &m, &bl, &log);
        let hi = source_rank_key_radio("ch", 3, "http://hi/a.m3u8", "http://hi", &m, &bl, &log);
        assert!(hi > mid && mid > lo, "kbps bucket dominates within radio sort");
    }

    #[test]
    fn audio_bucket_helpers_table() {
        assert_eq!(audio_kbps_bucket(Some(320)), 4);
        assert_eq!(audio_kbps_bucket(Some(192)), 3);
        assert_eq!(audio_kbps_bucket(Some(128)), 2);
        assert_eq!(audio_kbps_bucket(Some(64)), 1);
        assert_eq!(audio_kbps_bucket(Some(48)), 0);
        assert_eq!(audio_kbps_bucket(None), 0);
        assert_eq!(audio_sample_rate_bucket(Some(48000)), 3);
        assert_eq!(audio_sample_rate_bucket(Some(44100)), 2);
        assert_eq!(audio_sample_rate_bucket(Some(32000)), 1);
        assert_eq!(audio_sample_rate_bucket(Some(22050)), 0);
        assert_eq!(audio_channels_bucket(Some(2)), 2);
        assert_eq!(audio_channels_bucket(Some(1)), 1);
        assert_eq!(audio_channels_bucket(Some(0)), 0);
        assert_eq!(audio_channels_bucket(None), 0);
    }
}
