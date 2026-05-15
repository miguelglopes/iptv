use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::Serialize;
use time::OffsetDateTime;

use crate::canonical::quality_tier;
use crate::codec::Classification;
use crate::epg::{fetch_epg_for_channel, format_rtp_date, EpgCandidate};
use crate::state::AppState;
use crate::xtream::{ChannelKind, EpgProgram};

/// Map from canonical channel key (post-canonicalisation) to RTP's per-station
/// integer code in the radio EPG endpoint. Probed live against rtp.pt:
/// `/EPG/json/rtp-channels-page/list-grid/radio/{code}/{date}/lis`.
///
/// Codes empirically verified 2026-05-14 by program-name matching against
/// known shows. Channels not in this table get no EPG candidates and fall
/// back to the existing "no schedule info" empty state — same UX as any TV
/// channel without EPG.
fn rtp_radio_code(channel_key: &str) -> Option<u32> {
    match channel_key {
        "antena1" => Some(1),
        "antena2" => Some(2),
        "antena3" => Some(3),
        "rdpafrica" => Some(4),
        "rdpinternacional" => Some(5),
        _ => None,
    }
}

/// Derive `scheme://host[:port]` from the incoming request so URLs we emit
/// (play_url, segment URLs) resolve back to whichever address the client used
/// to reach us — LAN IP, public IP, reverse-proxy hostname, …. Prefers
/// `X-Forwarded-Host`/`X-Forwarded-Proto` so deployments behind a reverse
/// proxy advertise the public URL rather than the internal one. Falls back
/// to `Config::public_base_url` (and ultimately to `http://localhost:8080`)
/// only when no `Host` header is available.
pub fn request_base_url(headers: &HeaderMap, fallback: Option<&str>) -> String {
    let pick = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let host = pick("x-forwarded-host").or_else(|| {
        headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    let proto = pick("x-forwarded-proto").unwrap_or_else(|| "http".to_string());
    match host {
        Some(h) => format!("{proto}://{h}"),
        None => fallback
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "http://localhost:8080".to_string()),
    }
}

#[derive(Debug, Serialize)]
pub struct ChannelDto {
    pub key: String,
    pub name: String,
    pub kind: ChannelKind,
    /// Capability tags a client must support to play this channel.
    /// Generic string set; the server filter just checks set inclusion
    /// against `X-Client-Caps`. No special-casing per kind in the filter.
    pub caps_required: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_rank: Option<usize>,
    pub source_count: usize,
    pub play_url: String,
    pub tv_archive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tv_archive_duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tv_archive_quality: Option<&'static str>,
}

/// Map a channel kind to the capability tags a client needs to play it.
/// This is the *only* place kind influences the capability check — everything
/// else is generic set-inclusion logic. Adding a new stream type (DASH,
/// fMP4, …) means adding tags here and to the client's probe, nothing else.
///
/// Today every channel in the catalog is HLS / MPEG-TS / H.264 video + AAC
/// audio. If we ever surface HEVC or VP9 channels, declare them here:
/// e.g. `Tv4k => &["hls", "hevc", "aac"]`.
fn caps_required(kind: ChannelKind) -> &'static [&'static str] {
    match kind {
        // TV streams: HLS playlist with H.264/AAC MPEG-TS segments. Clients
        // need each codec; `live_video_hls` is the actual play-test cap
        // (proved by playing /api/probe/video.m3u8 against a real TV channel).
        ChannelKind::Tv => &["hls", "h264", "aac", "live_video_hls"],
        // Radio: HLS playlist (nested master→chunklist) with raw AAC segments.
        // No video. `live_audio_only_hls` is the play-test cap — Chrome's
        // native demuxer fails this shape; hls.js handles it.
        ChannelKind::Radio => &["hls", "aac", "live_audio_only_hls"],
    }
}

/// Parse the `X-Client-Caps` header into a set of cap tags. Missing header
/// means "no caps reported" — the filter falls back to permissive (returns
/// all channels) so older clients that don't probe still work.
fn parse_client_caps(headers: &HeaderMap) -> Option<std::collections::HashSet<String>> {
    let raw = headers.get("x-client-caps").and_then(|v| v.to_str().ok())?;
    Some(
        raw.split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

#[derive(Debug, Serialize)]
pub struct StatusDto {
    pub hosts: HostsStatusDto,
    pub catalog: CatalogStatusDto,
    pub epg: EpgStatusDto,
    pub blacklist: BlacklistStatusDto,
    pub classifier: ClassifierStatusDto,
}

#[derive(Debug, Serialize)]
pub struct ClassifierStatusDto {
    pub classified: usize,
    pub hevc: usize,
    pub with_subs: usize,
    pub entries: Vec<ClassifierEntryDto>,
}

#[derive(Debug, Serialize)]
pub struct ClassifierEntryDto {
    pub stream_id: u64,
    #[serde(flatten)]
    pub classification: Classification,
}

#[derive(Debug, Serialize)]
pub struct HostsStatusDto {
    pub total: usize,
    pub alive: usize,
    pub blacklisted: usize,
    pub details: Vec<crate::hosts::HostStatus>,
}

#[derive(Debug, Serialize)]
pub struct CatalogStatusDto {
    pub channels: usize,
    pub stream_count: usize,
    pub source_host: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_refreshed: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct EpgStatusDto {
    pub cached_channels: usize,
}

#[derive(Debug, Serialize)]
pub struct BlacklistStatusDto {
    pub failed_urls: usize,
    pub bad_hosts: usize,
    pub demoted_urls: usize,
}

pub async fn list_channels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let snap = state.catalog.snapshot();
    let base = request_base_url(&headers, state.config.public_base_url.as_deref());
    // Client-reported capability set (from `X-Client-Caps`). If absent, the
    // filter is a no-op — we keep every channel, same as before clients
    // started probing. `Some(set)` means the client explicitly listed its caps
    // and we should hide anything outside that set.
    let client_caps = parse_client_caps(&headers);
    type SortKey = (u8, usize, String, usize);
    let mut out: Vec<(SortKey, ChannelDto)> = snap
        .channels
        .iter()
        .enumerate()
        .filter(|(_, ch)| {
            // Generic set-inclusion: a channel's required caps must be a
            // subset of the client's reported caps. No special-case per kind.
            let required = caps_required(ch.kind);
            match &client_caps {
                None => true,
                Some(caps) => required.iter().all(|c| caps.contains(*c)),
            }
        })
        .map(|(orig_i, ch)| {
            let logo = ch.sources.iter().find_map(|s| s.logo.clone());
            // Pick the right curation per kind. Radio names live in their own
            // namespace (see [radio_curation] in config) so an "Antena 3" radio
            // doesn't borrow rank from a "Antena 3" TV channel that happened to
            // share the same canonical_key.
            let curation = match ch.kind {
                ChannelKind::Radio => &state.radio_curation,
                ChannelKind::Tv => &state.curation,
            };
            let d = curation.rank_of(&ch.key);
            let bucket: u8 = if d.is_some() { 0 } else { 1 };
            let sub = d.unwrap_or(usize::MAX);
            let archive_src = ch.pick_archive_source();
            let tv_archive = archive_src.is_some();
            let tv_archive_duration = archive_src.and_then(|s| s.tv_archive_duration);
            let tv_archive_quality = archive_src.and_then(|s| quality_tier(&s.name));
            let dto = ChannelDto {
                key: ch.key.clone(),
                name: ch.name.clone(),
                kind: ch.kind,
                caps_required: caps_required(ch.kind),
                logo,
                default_rank: d,
                source_count: ch.sources.len(),
                play_url: format!("{}/play/{}.m3u8", base.trim_end_matches('/'), ch.key),
                tv_archive,
                tv_archive_duration,
                tv_archive_quality,
            };
            let key: SortKey = (bucket, sub, dto.name.to_lowercase(), orig_i);
            (key, dto)
        })
        .collect();

    out.sort_by(|a, b| a.0.cmp(&b.0));

    let dtos: Vec<ChannelDto> = out.into_iter().map(|(_, d)| d).collect();
    Json(dtos)
}

pub async fn get_epg(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Vec<EpgProgram>>, (StatusCode, String)> {
    let snap = state.catalog.snapshot();
    let ch = snap
        .lookup(&key)
        .ok_or((StatusCode::NOT_FOUND, format!("unknown channel: {key}")))?;

    let cands: Vec<EpgCandidate> = match ch.kind {
        ChannelKind::Tv => {
            let alive = state.hosts.alive_hosts_ranked();
            if alive.is_empty() {
                return Err((StatusCode::SERVICE_UNAVAILABLE, "no alive hosts".into()));
            }
            let mut cs = Vec::new();
            for src in &ch.sources {
                let priority = if src.tv_archive { 1 } else { 0 };
                for host in &alive {
                    if state.blacklist.is_host_bad(host) {
                        continue;
                    }
                    cs.push(EpgCandidate::Xtream {
                        stream_id: src.stream_id,
                        host: host.clone(),
                        priority,
                    });
                }
            }
            cs
        }
        ChannelKind::Radio => {
            // Map canonical key → RTP radio code. Stations not in the table
            // emit zero candidates; the empty Vec triggers the existing
            // "no schedule info" empty state.
            match rtp_radio_code(&ch.key) {
                Some(code) => {
                    let now = OffsetDateTime::now_utc();
                    let today = now.date();
                    let tomorrow = today + time::Duration::days(1);
                    vec![
                        EpgCandidate::RtpRadio { code, date: format_rtp_date(today) },
                        EpgCandidate::RtpRadio { code, date: format_rtp_date(tomorrow) },
                    ]
                }
                None => Vec::new(),
            }
        }
    };

    let cached = fetch_epg_for_channel(
        &state.epg,
        &state.xtream,
        &state.upstream_http,
        &key,
        cands,
    )
    .await;
    Ok(Json(cached.programs))
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusDto> {
    let host_details = state.hosts.snapshot();
    let alive = host_details.iter().filter(|s| s.alive).count();
    let bl_hosts = state.blacklist.snapshot_hosts();
    let blacklisted = bl_hosts.len();
    let snap = state.catalog.snapshot();

    let classifier_entries: Vec<ClassifierEntryDto> = state
        .classifier
        .snapshot()
        .into_iter()
        .map(|(stream_id, classification)| ClassifierEntryDto { stream_id, classification })
        .collect();
    let hevc = classifier_entries
        .iter()
        .filter(|e| matches!(e.classification.video_codec, Some(crate::codec::VideoCodec::Hevc)))
        .count();
    let with_subs = classifier_entries
        .iter()
        .filter(|e| !e.classification.subtitle_pids.is_empty())
        .count();

    Json(StatusDto {
        hosts: HostsStatusDto {
            total: host_details.len(),
            alive,
            blacklisted,
            details: host_details,
        },
        catalog: CatalogStatusDto {
            channels: snap.channels.len(),
            stream_count: snap.stream_count,
            source_host: snap.source_host.clone(),
            last_refreshed: snap.last_refreshed,
        },
        epg: EpgStatusDto {
            cached_channels: state.epg.known_keys().len(),
        },
        blacklist: BlacklistStatusDto {
            failed_urls: state.blacklist.snapshot_urls().len(),
            bad_hosts: blacklisted,
            demoted_urls: state.blacklist.snapshot_demoted().len(),
        },
        classifier: ClassifierStatusDto {
            classified: classifier_entries.len(),
            hevc,
            with_subs,
            entries: classifier_entries,
        },
    })
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct FeedbackBody {
    #[serde(default)]
    pub kind: FeedbackKind,
    pub error: Option<String>,
    /// Play-id the client received in its play URL. When present we look up
    /// the exact upstream that was served to this play attempt — no LKG races
    /// when concurrent clients touch the same channel. Optional for backwards
    /// compatibility with older clients (those fall back to LKG-based blame).
    #[serde(default)]
    pub play_id: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedbackKind {
    #[default]
    Fail,
    Demote,
}

pub async fn feedback(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    body: Option<Json<FeedbackBody>>,
) -> StatusCode {
    let body = body.map(|b| b.0).unwrap_or_default();
    let error = body.error.unwrap_or_default();

    // Resolve which upstream to blame. Three states:
    //   - pid sent + session resolved + channel matches → blame that URL.
    //     LKG is dropped only when it still points at that same URL (so
    //     another client's LKG doesn't get evicted by accident).
    //   - pid sent + session not found or channel-mismatched → do NOTHING.
    //     The client explicitly identified an upstream; we shouldn't
    //     silently fall back to a different one (would re-introduce the
    //     race we're trying to fix). The threshold-based blacklist makes
    //     occasional lost signals harmless.
    //   - pid absent (legacy client) → drop LKG and blame that. Matches
    //     the pre-pid behaviour for back-compat with older client builds.
    let pid_was_supplied = body
        .play_id
        .as_deref()
        .map(|p| !p.trim().is_empty())
        .unwrap_or(false);
    let pid_for_log = body.play_id.clone();
    let blamed_via_pid = body.play_id.as_deref().and_then(|pid| {
        let (url, ch) = state.play_sessions.lookup(pid)?;
        if ch == key {
            Some(url)
        } else {
            // pid mapped to a different channel — either stale or forged.
            tracing::warn!(
                channel = %key,
                pid = %pid,
                pid_channel = %ch,
                "feedback pid mismatched channel; ignoring"
            );
            None
        }
    });

    let blamed: Option<String> = match (pid_was_supplied, blamed_via_pid) {
        (_, Some(url)) => {
            state.blacklist.drop_last_known_good_if_matches(&key, &url);
            Some(url)
        }
        (true, None) => {
            tracing::info!(
                channel = %key,
                pid = ?pid_for_log,
                "feedback pid unknown (session expired or never recorded); not falling back to LKG"
            );
            None
        }
        (false, None) => state.blacklist.drop_last_known_good(&key),
    };

    match body.kind {
        FeedbackKind::Fail => {
            if let Some(url) = blamed.as_deref() {
                // mark_failed = demote_url + note_url_failed. The demote
                // happens unconditionally so the next play prefers something
                // else; the windowed fail count drives hard blacklist after
                // crossing url_fail_threshold.
                state.blacklist.mark_failed(url);
                tracing::info!(
                    channel = %key,
                    url = %url,
                    pid = ?pid_for_log,
                    error = %error,
                    "client-reported failure: demoted + counted"
                );
            } else {
                tracing::info!(channel = %key, error = %error, "client-reported failure: nothing to blame");
            }
        }
        FeedbackKind::Demote => {
            if let Some(url) = blamed.as_deref() {
                state.blacklist.demote_url(url);
                tracing::info!(
                    channel = %key,
                    url = %url,
                    pid = ?pid_for_log,
                    error = %error,
                    "client-reported demote: deprioritized current pick"
                );
            } else {
                tracing::info!(channel = %key, error = %error, "client-reported demote: nothing to demote");
            }
        }
    }
    StatusCode::NO_CONTENT
}

pub async fn admin_reprobe(State(state): State<Arc<AppState>>) -> StatusCode {
    state.hosts.request_reprobe();
    state.catalog.request_refresh();
    StatusCode::ACCEPTED
}

pub async fn admin_clear_blacklist(State(state): State<Arc<AppState>>) -> StatusCode {
    state.blacklist.clear_blacklist();
    StatusCode::NO_CONTENT
}

pub async fn admin_clear_demoted(State(state): State<Arc<AppState>>) -> StatusCode {
    state.blacklist.clear_demoted();
    StatusCode::NO_CONTENT
}

pub async fn admin_clear_all(State(state): State<Arc<AppState>>) -> StatusCode {
    state.blacklist.clear_all();
    state.classifier.clear();
    StatusCode::NO_CONTENT
}

pub async fn admin_clear_classifier(State(state): State<Arc<AppState>>) -> StatusCode {
    state.classifier.clear();
    StatusCode::NO_CONTENT
}

pub async fn admin_recent_plays(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.play_log.snapshot())
}

/// Capability probe: redirect to a playable channel of the requested kind so
/// the client can run a real play-test against the actual content shape we
/// serve.
///
/// Used to pick a single "curation-top" channel, which made the probe a SPOF:
/// if that channel was temporarily dead the whole mode disappeared from the
/// client. Now we scan curation-ranked channels and pick the first one whose
/// sources still touch an alive, non-blacklisted host (or for radio, a
/// non-blacklisted direct_source URL). Falls back to the top channel by raw
/// curation rank when nothing passes — better to probe a possibly-stale
/// channel than to fail the boot's capability detection entirely.
async fn probe_redirect(
    state: &AppState,
    headers: &HeaderMap,
    kind: ChannelKind,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let snap = state.catalog.snapshot();
    let curation = match kind {
        ChannelKind::Tv => &state.curation,
        ChannelKind::Radio => &state.radio_curation,
    };
    let alive: std::collections::HashSet<String> =
        state.hosts.alive_hosts_ranked().into_iter().collect();
    let mut ranked: Vec<&crate::canonical::CanonicalChannel> = snap
        .channels
        .iter()
        .filter(|c| c.kind == kind)
        .collect();
    ranked.sort_by_key(|c| curation.rank_of(&c.key).unwrap_or(usize::MAX));
    if ranked.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("no {kind:?} channel available for probe"),
        ));
    }
    let healthy = ranked.iter().find(|c| channel_has_alive_source(state, &alive, c));
    let ch = healthy.copied().unwrap_or(ranked[0]);

    let base = request_base_url(headers, state.config.public_base_url.as_deref());
    let target = format!(
        "{}/play/{}.m3u8?probe=1",
        base.trim_end_matches('/'),
        ch.key
    );
    let mut resp = axum::response::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .body(axum::body::Body::empty())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("build redirect: {e}"),
            )
        })?;
    resp.headers_mut().insert(
        axum::http::header::LOCATION,
        axum::http::HeaderValue::from_str(&target).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("bad redirect url: {e}"),
            )
        })?,
    );
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(resp)
}

fn channel_has_alive_source(
    state: &AppState,
    alive: &std::collections::HashSet<String>,
    ch: &crate::canonical::CanonicalChannel,
) -> bool {
    ch.sources.iter().any(|src| {
        if let Some(direct) = &src.direct_source {
            !state.blacklist.is_url_failed(direct)
        } else if !src.origin_host.is_empty() {
            alive.contains(&src.origin_host)
                && !state.blacklist.is_host_bad(&src.origin_host)
        } else {
            alive
                .iter()
                .any(|h| !state.blacklist.is_host_bad(h))
        }
    })
}

pub async fn probe_video(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_redirect(&state, &headers, ChannelKind::Tv).await
}

pub async fn probe_audio(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_redirect(&state, &headers, ChannelKind::Radio).await
}

/// Serve `index.html` with a player bundle picked per User-Agent. webOS
/// Chromium plays HLS natively (and chokes on radio's nested audio-only HLS
/// in some non-webOS browsers — confirmed by empirical matrix testing); other
/// browsers (Chrome/Firefox on desktop) get `hls.js` injected so their MSE
/// path handles every HLS shape uniformly. The HTML template carries a single
/// marker `<!--PLAYER_BUNDLE_MARKER-->` that gets replaced.
///
/// This is the cleanest place for the decision: server inspects UA at the
/// HTML edge and sends a per-client bundle, rather than every client carrying
/// a UA-check in JS and document.write-ing a script. Same pattern as
/// `request_base_url` does for the play_url scheme/host.
pub async fn serve_index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::http::HeaderValue;
    let path = state.config.ui_dir.join("index.html");
    let body = std::fs::read_to_string(&path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read index: {e}")))?;
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ua_lower = ua.to_ascii_lowercase();
    let is_webos = ua_lower.contains("web0s") || ua_lower.contains("webos");
    let injection = if is_webos {
        // webOS path: leave the marker empty — `<video>.src = url` decodes HLS
        // via the system media stack. Skipping the ~400 KB hls.min.js keeps
        // boot fast on TV.
        ""
    } else {
        r#"<script src="lib/hls.min.js"></script>"#
    };
    let rendered = body.replace("<!--PLAYER_BUNDLE_MARKER-->", injection);
    let mut resp = Html(rendered).into_response();
    // index.html is now dynamic (UA-templated) — never let a browser or proxy
    // serve a cached copy that might have the wrong injection for a different
    // client. Same hygiene the play_playlist handler uses for its m3u8 output.
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn base_url_uses_host_header_with_default_scheme() {
        assert_eq!(
            request_base_url(&hm(&[("host", "192.163.2.90:8080")]), None),
            "http://192.163.2.90:8080"
        );
    }

    #[test]
    fn base_url_prefers_x_forwarded_host_and_proto() {
        // Behind a reverse proxy: Host is the internal name, X-Forwarded-* carry
        // the public URL the client actually used.
        let headers = hm(&[
            ("host", "iptv-proxy.internal:8080"),
            ("x-forwarded-host", "iptv.example.com"),
            ("x-forwarded-proto", "https"),
        ]);
        assert_eq!(request_base_url(&headers, None), "https://iptv.example.com");
    }

    #[test]
    fn base_url_picks_first_value_in_xff_list() {
        // Proxies may chain "client, proxy1, proxy2" — first entry is closest
        // to the actual client.
        let headers = hm(&[
            ("x-forwarded-host", "iptv.example.com, internal-lb:8080"),
            ("x-forwarded-proto", "https, http"),
        ]);
        assert_eq!(request_base_url(&headers, None), "https://iptv.example.com");
    }

    #[test]
    fn base_url_falls_back_to_config_when_host_missing() {
        // HTTP/1.0 clients or programmatic callers may omit Host. Fallback only
        // kicks in then.
        assert_eq!(
            request_base_url(&hm(&[]), Some("http://192.163.2.90:8080")),
            "http://192.163.2.90:8080"
        );
        // Trailing slash trimmed for consistency with the format!() callsites
        // that already append "/play/...".
        assert_eq!(
            request_base_url(&hm(&[]), Some("http://192.163.2.90:8080/")),
            "http://192.163.2.90:8080"
        );
    }

    #[test]
    fn base_url_final_fallback_to_localhost() {
        // No Host, no config — only happens with a non-conformant client and a
        // bare config; we still return *something* usable for local debugging.
        assert_eq!(request_base_url(&hm(&[]), None), "http://localhost:8080");
    }
}
