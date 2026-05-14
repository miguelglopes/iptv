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

use crate::canonical::{CanonicalChannel, CanonicalSource};
use crate::codec::{classify_ts_chunk, strip_subtitle_pids};
use crate::state::AppState;

const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

#[derive(Debug, Clone)]
struct Candidate {
    url: String,
    host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentToken {
    u: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    p: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    c: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PlayParams {
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
}

pub async fn play_playlist(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(params): Query<PlayParams>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let key = name.trim_end_matches(".m3u8").trim_end_matches(".ts");
    let catchup_request = parse_catchup_params(&params)?;
    let public_base = request_base_url(&headers, state.config.public_base_url.as_deref());

    let snap = state.catalog.snapshot();
    let channel = snap
        .lookup(key)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, format!("unknown channel: {key}")))?;

    if let Some(req) = catchup_request {
        return catchup_play(state, channel, req, &public_base).await;
    }

    let candidates = build_candidates(&state, &channel);
    if candidates.is_empty() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no candidate sources for channel".into()));
    }

    let budget = Duration::from_secs(state.config.proxy.play_budget_secs);
    let per_attempt = Duration::from_secs(state.config.proxy.per_attempt_timeout_secs);
    let started = Instant::now();

    let mut last_err: Option<String> = None;
    let mut tried = 0usize;

    for (idx, cand) in candidates.iter().enumerate() {
        let elapsed = started.elapsed();
        if elapsed >= budget {
            warn!("play budget exhausted for {} after {} attempts", channel.key, idx);
            break;
        }
        let remaining = budget.saturating_sub(elapsed);
        let attempt_timeout = per_attempt.min(remaining);
        tried += 1;
        match tokio::time::timeout(
            attempt_timeout,
            fetch_and_rewrite_playlist(&state, &channel, cand, &public_base),
        )
        .await
        {
            Ok(Ok(resp)) => {
                state.blacklist.note_url_succeeded(&channel.key, &cand.url);
                schedule_opportunistic_validation(
                    Arc::clone(&state),
                    channel.key.clone(),
                    &candidates,
                    idx,
                );
                return Ok(resp);
            }
            Ok(Err(e)) => {
                warn!("playlist fetch failed for {}: {} → {}", channel.key, cand.url, e);
                state.blacklist.note_url_failed(&cand.url);
                last_err = Some(e.to_string());
                continue;
            }
            Err(_) => {
                warn!(
                    "playlist fetch timed out for {} on {} after {:?}",
                    channel.key, cand.url, attempt_timeout
                );
                state.blacklist.note_url_failed(&cand.url);
                last_err = Some(format!("timeout after {attempt_timeout:?}"));
                continue;
            }
        }
    }

    Err((
        StatusCode::BAD_GATEWAY,
        format!(
            "all {tried}/{total} candidates failed for {channel} (last: {err})",
            total = candidates.len(),
            channel = channel.key,
            err = last_err.as_deref().unwrap_or("budget exhausted"),
        ),
    ))
}

fn build_candidates(state: &AppState, channel: &CanonicalChannel) -> Vec<Candidate> {
    let alive = state.hosts.alive_hosts_ranked();
    let mut fresh: Vec<Candidate> = Vec::new();
    let mut demoted: Vec<Candidate> = Vec::new();

    for src in &channel.sources {
        // Skip sources known to be undecodable on this client (webOS B4: HEVC).
        // Stream-id-level classification: one bad PMT classification skips the
        // source across all hosts.
        if let Some(c) = state.classifier.get(src.stream_id) {
            if c.unplayable_on_webos_b4() {
                continue;
            }
        }
        for host in &alive {
            if state.blacklist.is_host_bad(host) {
                continue;
            }
            let url = state.xtream.stream_url(host, src.stream_id, "m3u8");
            if state.blacklist.is_url_failed(&url) {
                continue;
            }
            let cand = Candidate { url: url.clone(), host: host.clone() };
            if state.blacklist.is_url_demoted(&url) {
                demoted.push(cand);
            } else {
                fresh.push(cand);
            }
        }
    }

    if let Some(lkg) = state.blacklist.last_known_good(&channel.key) {
        let demoted_lkg = state.blacklist.is_url_demoted(&lkg);
        if let Some(pos) = fresh.iter().position(|c| c.url == lkg) {
            let item = fresh.remove(pos);
            fresh.insert(0, item);
        } else if !demoted_lkg && !state.blacklist.is_url_failed(&lkg) {
            let host = derive_host(&lkg).unwrap_or_default();
            fresh.insert(0, Candidate { url: lkg, host });
        }
    }

    fresh.extend(demoted);

    // If the blacklist filtered everything out, fall back to the unfiltered
    // source × alive_host matrix. The blacklist is a hint, not a hard
    // rule — failing the request without trying anything is worse than
    // probing a possibly-stale entry. If they really are all dead, the
    // attempt loop in `play_playlist` returns 502 within its budget.
    if fresh.is_empty() && !alive.is_empty() {
        for src in &channel.sources {
            for host in &alive {
                let url = state.xtream.stream_url(host, src.stream_id, "m3u8");
                fresh.push(Candidate { url, host: host.clone() });
            }
        }
    }

    fresh
}

async fn fetch_and_rewrite_playlist(
    state: &AppState,
    channel: &CanonicalChannel,
    cand: &Candidate,
    public_base: &str,
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
        let rewritten = rewrite_playlist(
            body,
            &final_url,
            public_base,
            &channel.key,
            &cand.url,
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
) -> anyhow::Result<String> {
    let base = playlist_url.clone();
    let public_base = public_base.trim_end_matches('/');
    let mut out = String::with_capacity(body.len() + 256);

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        if trimmed.starts_with('#') {
            if let Some(rewritten) = rewrite_tag_with_uri(trimmed, &base, public_base, channel_key, source_url) {
                out.push_str(&rewritten);
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if let Ok(resolved) = base.join(trimmed) {
            out.push_str(&proxy_url(public_base, resolved.as_str(), channel_key, source_url));
            out.push('\n');
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
) -> Option<String> {
    let uri_marker = "URI=\"";
    let idx = line.find(uri_marker)?;
    let start = idx + uri_marker.len();
    let rel_end = line[start..].find('"')?;
    let raw = &line[start..start + rel_end];
    let resolved = base.join(raw).ok()?;
    let new_uri = proxy_url(public_base, resolved.as_str(), channel_key, source_url);
    let mut s = String::with_capacity(line.len() + new_uri.len());
    s.push_str(&line[..start]);
    s.push_str(&new_uri);
    s.push_str(&line[start + rel_end..]);
    Some(s)
}

fn proxy_url(public_base: &str, absolute_upstream: &str, channel_key: &str, source_url: &str) -> String {
    let payload = SegmentToken {
        u: absolute_upstream.to_string(),
        p: Some(source_url.to_string()),
        c: Some(channel_key.to_string()),
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
        .trim_end_matches(".m4s");
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
    Ok(SegmentToken { u: upstream, p: None, c: None })
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
    let is_ts = content_type.contains("mp2t") || upstream_path.ends_with(".ts");
    let stream_id = segment.p.as_deref().and_then(stream_id_from_source_url);

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
        let processed = handle_ts_segment(&state, stream_id.unwrap(), &bytes);
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

fn handle_ts_segment(state: &AppState, stream_id: u64, bytes: &Bytes) -> Vec<u8> {
    let classification = match state.classifier.get(stream_id) {
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
    state.blacklist.note_url_failed(&segment.u);
    if let Some(source_url) = segment.p.as_deref() {
        state.blacklist.note_url_failed(source_url);
        if let Some(channel_key) = segment.c.as_deref() {
            if let Some(lkg) = state.blacklist.last_known_good(channel_key) {
                if lkg == source_url {
                    state.blacklist.drop_last_known_good(channel_key);
                }
            }
        }
    }
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
                        state.blacklist.note_url_failed(&cand.url);
                        continue;
                    }
                    if is_abuse_url(&final_url) {
                        warn!(
                            channel = %channel_key,
                            url = %cand.url,
                            "opportunistic validation: abuse redirect"
                        );
                        state.blacklist.note_url_failed(&cand.url);
                        continue;
                    }
                    match resp.bytes().await {
                        Ok(bytes) => {
                            let head = std::str::from_utf8(bytes.get(..7).unwrap_or(&[])).unwrap_or("");
                            if head.starts_with("#EXTM3U") {
                                debug!(channel = %channel_key, url = %cand.url, "opportunistic validation: ok");
                            } else {
                                warn!(
                                    channel = %channel_key,
                                    url = %cand.url,
                                    "opportunistic validation: not a playlist"
                                );
                                state.blacklist.note_url_failed(&cand.url);
                            }
                        }
                        Err(e) => {
                            warn!(
                                channel = %channel_key,
                                url = %cand.url,
                                err = %e,
                                "opportunistic validation: body read failed"
                            );
                            state.blacklist.note_url_failed(&cand.url);
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
                    state.blacklist.note_url_failed(&cand.url);
                }
                Err(_) => {
                    warn!(
                        channel = %channel_key,
                        url = %cand.url,
                        "opportunistic validation: timeout"
                    );
                    state.blacklist.note_url_failed(&cand.url);
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
) -> Result<Response, (StatusCode, String)> {
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

    let host = pick_archive_host(&state, &channel, source).ok_or((
        StatusCode::BAD_GATEWAY,
        "catch-up upstream failed: no alive host".into(),
    ))?;

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

    // Catch-up has a single source; the per-attempt timeout used for live
    // failover doesn't apply. Lean on the upstream_http client's own timeout.
    let cand = Candidate { url: upstream.clone(), host };
    match fetch_and_rewrite_playlist(&state, &channel, &cand, public_base).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            warn!(channel = %channel.key, url = %cand.url, error = %e, "catch-up upstream failed");
            Err((
                StatusCode::BAD_GATEWAY,
                format!("catch-up upstream failed: {e}"),
            ))
        }
    }
}

fn pick_archive_host(
    state: &AppState,
    _channel: &CanonicalChannel,
    _source: &CanonicalSource,
) -> Option<String> {
    let alive = state.hosts.alive_hosts_ranked();
    alive
        .into_iter()
        .find(|h| !state.blacklist.is_host_bad(h))
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
        let ch = CanonicalChannel {
            key: "x".into(),
            name: "X".into(),
            sources: vec![
                CanonicalSource {
                    stream_id: 1,
                    name: "HD".into(),
                    score: 15,
                    logo: None,
                    tv_archive: true,
                    tv_archive_duration: Some(3),
                },
                CanonicalSource {
                    stream_id: 2,
                    name: "4K".into(),
                    score: 30,
                    logo: None,
                    tv_archive: true,
                    tv_archive_duration: Some(3),
                },
                CanonicalSource {
                    stream_id: 3,
                    name: "RAW".into(),
                    score: 40,
                    logo: None,
                    tv_archive: false,
                    tv_archive_duration: None,
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
        let ch = CanonicalChannel {
            key: "x".into(),
            name: "X".into(),
            sources: vec![CanonicalSource {
                stream_id: 1,
                name: "RAW".into(),
                score: 40,
                logo: None,
                tv_archive: false,
                tv_archive_duration: None,
            }],
        };
        assert!(ch.pick_archive_source().is_none());
    }
}
