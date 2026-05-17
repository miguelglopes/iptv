use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::Serialize;
use time::OffsetDateTime;

use crate::canonical::{quality_tier, CanonicalChannel};
use crate::codec::Classification;
use crate::epg::{fetch_epg_for_channel, format_rtp_date, EpgCandidate};
use crate::radio::RadioFormat;
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
    /// Capability tags a client must support to play this channel. Per-channel
    /// for radio (driven by `RadioFormat`) so a pure-MP3 station isn't sent
    /// to a client that doesn't probe `mp3`. JSON shape unchanged for older
    /// clients — still a string array. Under `caps_v2_per_variant=true`,
    /// emits the rank-winner variant's caps so client eviction attributes
    /// failures to the right tag (h264_excess_refs vs h264).
    pub caps_required: Vec<String>,
    /// Audio container/transport, only set for radio. Informational; the
    /// client decides hls.js vs native via the `play_url` extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
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

// Per-channel caps_required derivation lives in `caps_cache::caps_required`
// (Phase 6). The cache invalidates on catalog refresh, MeasuredStore generation
// bump, and alive-hosts change. Listed at the bottom of `list_channels`.

fn radio_format_label(fmt: RadioFormat) -> &'static str {
    match fmt {
        RadioFormat::Hls => "hls",
        RadioFormat::Mp3 => "mp3",
        RadioFormat::Aac => "aac",
        RadioFormat::Icecast => "icecast",
        RadioFormat::Playlist => "playlist",
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
    /// Total number of URLs the state machine is tracking — any URL with
    /// recorded cool-off / failure / heartbeat history. Replaces the old
    /// `failed_urls + demoted_urls` split now that those two buckets are
    /// unified into a single per-URL cool-off step (Phase 2).
    pub url_states_count: usize,
    pub bad_hosts: usize,
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

    // Phase 6: per-channel caps_required cache. The lookup map is the
    // tightened cap list per channel; the version string is the cap-matrix
    // digest emitted as `X-Caps-Matrix-Version` so the client can re-probe
    // when the server-side cap surface shifts.
    let alive_hosts = state.hosts.alive_hosts_ranked();
    let caps_snap = state.caps_cache.ensure_with_v2(
        &snap,
        &state.measured,
        &alive_hosts,
        &state.curation,
        Some(&state.blacklist),
        state.config.caps_v2_stale_secs,
        state.config.caps_v2_per_variant,
    );
    let baseline_tv: Vec<&'static str> = vec!["hls", "h264", "aac", "live_video_hls"];
    let v2 = state.config.caps_v2_per_variant;
    // Phase 2: per-channel rank-winner caps under v2. When v2 is on we
    // recompute per request (cheap — O(variants × hosts)); when off, fall
    // back to the cached per-channel map.
    let client_caps_vec: Option<Vec<String>> = client_caps
        .as_ref()
        .map(|s| s.iter().cloned().collect());
    let v2_caps_for = |ch: &CanonicalChannel| -> Option<Vec<String>> {
        crate::caps_cache::channel_caps_v2(
            &state,
            ch,
            client_caps_vec.as_deref(),
        )
    };
    let channel_caps_static = |ch: &CanonicalChannel| -> Vec<&'static str> {
        caps_snap
            .per_channel
            .get(&ch.key)
            .cloned()
            .unwrap_or_else(|| baseline_tv.clone())
    };
    let channel_caps_strings = |ch: &CanonicalChannel| -> Vec<String> {
        if v2 {
            if let Some(c) = v2_caps_for(ch) {
                return c;
            }
            // Variant scope empty / no survivor → fall through to caller's
            // visible filter (which drops the channel).
            return Vec::new();
        }
        channel_caps_static(ch)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    };

    let mut hidden_by_caps_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let visible: Vec<(usize, &CanonicalChannel)> = snap
        .channels
        .iter()
        .enumerate()
        .filter(|(_, ch)| {
            let required: Vec<String> = channel_caps_strings(ch);
            // Under v2: empty required means the channel has no surviving
            // variant for this client — hide it.
            if v2 && required.is_empty() {
                hidden_by_caps_counts
                    .entry("no-survivor".to_string())
                    .and_modify(|n| *n += 1)
                    .or_insert(1);
                return false;
            }
            match &client_caps {
                None => true,
                Some(caps) => {
                    let satisfied = required.iter().all(|c| caps.contains(c));
                    if !satisfied {
                        // Attribute to the first missing tag.
                        for c in &required {
                            if !caps.contains(c) {
                                hidden_by_caps_counts
                                    .entry(c.clone())
                                    .and_modify(|n| *n += 1)
                                    .or_insert(1);
                                break;
                            }
                        }
                    }
                    satisfied
                }
            }
        })
        .collect();

    // Cross-channel uniqueness map: how many distinct channels reference each
    // logo URL. Upstreams sometimes return a generic operator placeholder
    // (e.g. the MEO "M" circle) as `stream_icon` for channels they don't have
    // a proper icon for, leaving many unrelated channels sharing one URL.
    // Counting per-channel (HashSet dedup within a channel's own sources)
    // lets us prefer a unique-to-this-channel logo over a shared placeholder
    // and outright drop heavily-shared URLs as placeholders.
    let mut logo_url_freq: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for (_, ch) in &visible {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for s in &ch.sources {
            if let Some(url) = s.logo.as_deref() {
                if seen.insert(url) {
                    *logo_url_freq.entry(url).or_insert(0) += 1;
                }
            }
        }
    }
    // Above this many distinct channels referencing the same URL = placeholder.
    // Empirical: catalog shows clusters of 33/99/144 channels sharing one
    // operator placeholder while legitimate channel-family logos (DAZN, etc.)
    // top out at ~6. 10 splits cleanly.
    const PLACEHOLDER_THRESHOLD: usize = 10;

    type SortKey = (u8, usize, String, usize);
    let mut out: Vec<(SortKey, ChannelDto)> = visible
        .into_iter()
        .map(|(orig_i, ch)| {
            let curation = match ch.kind {
                ChannelKind::Radio => &state.radio_curation,
                ChannelKind::Tv => &state.curation,
            };
            // curation.logo_overrides wins absolutely — for channels whose
            // upstream stream_icon is just wrong (e.g. CM TV → MCM TOP image)
            // and uniqueness filtering can't help (the wrong URL is unique
            // to this channel). Falls back to: pick the source whose logo
            // URL is referenced by the fewest other channels (ties broken by
            // source order = score). If even the minimum is above the
            // placeholder threshold, drop the logo entirely — showing the
            // channel name alone beats showing the wrong logo.
            let logo = curation.logo_overrides.get(&ch.key).cloned().or_else(|| {
                ch.sources
                    .iter()
                    .filter_map(|s| {
                        s.logo
                            .as_ref()
                            .map(|u| (logo_url_freq.get(u.as_str()).copied().unwrap_or(0), u))
                    })
                    .min_by_key(|(count, _)| *count)
                    .filter(|(count, _)| *count <= PLACEHOLDER_THRESHOLD)
                    .map(|(_, u)| u.clone())
            });
            let d = curation.rank_of(&ch.key);
            let bucket: u8 = if d.is_some() { 0 } else { 1 };
            let sub = d.unwrap_or(usize::MAX);
            let archive_src = ch.pick_archive_source();
            let tv_archive = archive_src.is_some();
            let tv_archive_duration = archive_src.and_then(|s| s.tv_archive_duration);
            let tv_archive_quality = archive_src.and_then(|s| quality_tier(&s.name));
            // Pick the URL extension per channel kind/format. HLS radios stay
            // on `.m3u8` so the existing manifest-rewriting pipeline handles
            // them; non-HLS radio uses `.audio` so the client's hls.js gate
            // (`/\.m3u8(\?|$)/` in app/js/player.js) falls through to native
            // `<video src>` (which on webOS decodes raw MP3/AAC over HTTP).
            let (play_ext, format) = match ch.kind {
                ChannelKind::Tv => (".m3u8", None),
                ChannelKind::Radio => {
                    let fmt = ch
                        .sources
                        .iter()
                        .find_map(|s| s.radio_format)
                        .unwrap_or(RadioFormat::Hls);
                    let ext = match fmt {
                        RadioFormat::Hls => ".m3u8",
                        _ => ".audio",
                    };
                    (ext, Some(radio_format_label(fmt)))
                }
            };
            let dto = ChannelDto {
                key: ch.key.clone(),
                name: ch.name.clone(),
                kind: ch.kind,
                caps_required: channel_caps_strings(ch),
                format,
                logo,
                default_rank: d,
                source_count: ch.sources.len(),
                play_url: format!("{}/play/{}{}", base.trim_end_matches('/'), ch.key, play_ext),
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
    let mut resp = Json(dtos).into_response();
    // Phase 6: cap-matrix version header. Clients store this in
    // localStorage; on mismatch they clear their cached cap set and
    // re-probe before issuing the next /api/channels request, ensuring
    // the freshness-loop-driven server-side tightening doesn't silently
    // hide channels while the client still believes the looser cap set.
    if let Ok(v) = axum::http::HeaderValue::from_str(&caps_snap.version) {
        resp.headers_mut().insert("x-caps-matrix-version", v);
    }
    // Phase 4: X-Probes-Expected so the client save-guard knows which
    // tags every probe must have resolved (true/false, not indeterminate)
    // before persisting to localStorage. Per-state, not per-request —
    // recomputed once per `/api/channels` call.
    if v2 {
        let expected = crate::caps_cache::probes_expected(
            &snap,
            &state.measured,
            &state.blacklist,
            &alive_hosts,
            state.config.caps_v2_stale_secs,
        );
        if let Ok(v) = axum::http::HeaderValue::from_str(&expected.join(",")) {
            resp.headers_mut().insert("x-probes-expected", v);
        }
    }
    *state.channels_hidden_by_caps.write() = hidden_by_caps_counts;
    resp
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
            url_states_count: state.blacklist.per_url_count(),
            bad_hosts: blacklisted,
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
    /// Playback phase when the failure happened: `"pre-canplay"` (player
    /// never reached readyState >= 2 — slow-to-start, watchdog, manifest
    /// fetch error) or `"post-canplay"` (mid-stream decoder error after
    /// the first frame). Optional for backwards compatibility with older
    /// clients that didn't send it. Logged here; the state machine in a
    /// later step consumes it to distinguish ClientPreCanplay (log-only)
    /// from ClientPostCanplay (cool-off bump).
    #[serde(default)]
    pub phase: Option<String>,
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
    use crate::blacklist::FailureKind;

    let body = body.map(|b| b.0).unwrap_or_default();
    let error = body.error.unwrap_or_default();
    let phase = body.phase.as_deref().unwrap_or("");

    // Resolve which upstream to blame via pid. Two states (Step 4 dropped
    // the LKG-fallback path along with `drop_last_known_good*`):
    //   - pid resolves + channel matches → blame that URL.
    //   - pid missing / unresolved / channel-mismatched → log and stop. The
    //     state machine forgives the lost signal at the next heartbeat
    //     cycle; this is far safer than blindly bumping an arbitrary URL.
    let pid_for_log = body.play_id.clone();
    let blamed: Option<String> = body.play_id.as_deref().and_then(|pid| {
        let (url, ch) = state.play_sessions.lookup(pid)?;
        if ch == key {
            Some(url)
        } else {
            tracing::warn!(
                channel = %key,
                pid = %pid,
                pid_channel = %ch,
                "feedback pid mismatched channel; ignoring"
            );
            None
        }
    });

    match body.kind {
        FeedbackKind::Fail => {
            // Phase routes to the state-machine variant: post-canplay = real
            // mid-playback decoder error (cool-off bump, no host blame);
            // pre-canplay or absent = slow-to-start (architecture.md §4 says
            // this is NOT an instability signal — log-only, no state mutation).
            let kind = match phase {
                "post-canplay" => FailureKind::ClientPostCanplay,
                _ => FailureKind::ClientPreCanplay,
            };
            if let Some(url) = blamed.as_deref() {
                state.blacklist.note_failure(url, kind);
                tracing::info!(
                    channel = %key,
                    url = %url,
                    pid = ?pid_for_log,
                    error = %error,
                    phase = %phase,
                    kind = ?kind,
                    "client-reported failure"
                );
            } else {
                tracing::info!(
                    channel = %key,
                    error = %error,
                    phase = %phase,
                    kind = ?kind,
                    "client-reported failure: no pid blame"
                );
            }
        }
        FeedbackKind::Demote => {
            // User pressed Green to deprioritise. One cool-off step bump,
            // no host blame (it's a user preference, not an upstream signal).
            // Modelled as a ClientPostCanplay variant (same semantics: bump
            // step without blaming the host).
            if let Some(url) = blamed.as_deref() {
                state.blacklist.note_failure(url, FailureKind::ClientPostCanplay);
                tracing::info!(
                    channel = %key,
                    url = %url,
                    pid = ?pid_for_log,
                    error = %error,
                    phase = %phase,
                    "client-reported demote (user Green)"
                );
            } else {
                tracing::info!(
                    channel = %key,
                    error = %error,
                    phase = %phase,
                    "client-reported demote: no pid blame"
                );
            }
        }
    }
    StatusCode::NO_CONTENT
}

/// Body for `POST /api/heartbeat` — see `heartbeat` below.
#[derive(Debug, Default, serde::Deserialize)]
pub struct HeartbeatBody {
    /// Per-play identifier the client baked into its play URL. Resolved
    /// here to `(channel, upstream_url)` via `state.play_sessions` so the
    /// heartbeat is attributed to the exact upstream that was served.
    #[serde(default)]
    pub play_id: Option<String>,
}

/// Clean-play heartbeat from the client. Fires every 30 s while playback is
/// healthy (player.js arms a setInterval on `canplay` and clears it on
/// stop/teardown/error). Used by the blacklist state machine (Phase 2's
/// `note_heartbeat`) to reset the cool-off step once the URL has been clean
/// for `clean_play_reset_secs` with fresh heartbeats.
///
/// Quiet on missing/expired play_id — legacy clients that don't send pid,
/// or session entries that TTL'd out before the first heartbeat, should
/// not surface as errors. Returns 204 either way.
pub async fn heartbeat(
    State(state): State<Arc<AppState>>,
    body: Option<Json<HeartbeatBody>>,
) -> StatusCode {
    let body = body.map(|b| b.0).unwrap_or_default();
    let Some(pid) = body.play_id.as_deref().filter(|p| !p.trim().is_empty()) else {
        tracing::trace!("heartbeat without play_id; ignoring");
        return StatusCode::NO_CONTENT;
    };
    match state.play_sessions.lookup(pid) {
        Some((url, channel)) => {
            state.blacklist.note_heartbeat(&url);
            tracing::trace!(channel = %channel, pid = %pid, url = %url, "heartbeat");
        }
        None => {
            tracing::trace!(pid = %pid, "heartbeat for unknown/expired pid; ignoring");
        }
    }
    StatusCode::NO_CONTENT
}

pub async fn admin_reprobe(State(state): State<Arc<AppState>>) -> StatusCode {
    state.hosts.request_reprobe();
    state.catalog.request_refresh();
    StatusCode::ACCEPTED
}

pub async fn admin_clear_classifier(State(state): State<Arc<AppState>>) -> StatusCode {
    state.classifier.clear();
    StatusCode::NO_CONTENT
}

/// Test-only sample injection. Wired into the router only when the server
/// is started with `IPTV_TEST_HOOKS=1`. Phase 10 e2e specs use this to
/// seed a measured-quality entry without driving real upstream traffic —
/// the alternative is committing AAC/TS fixture binaries and a
/// docker-compose harness, neither of which the project wants.
///
/// Body shape: a serde-deserialisable `Sample` (matches the on-disk
/// `measured.rs::Sample` schema). The endpoint pushes it into
/// `state.measured` under the given `(stream_id, host)` key.
#[derive(serde::Deserialize)]
pub struct InjectSampleBody {
    pub stream_id: u64,
    pub host: String,
    pub sample: crate::measured::Sample,
}

pub async fn admin_inject_sample(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InjectSampleBody>,
) -> StatusCode {
    state.measured.push(body.stream_id, &body.host, body.sample);
    StatusCode::NO_CONTENT
}

pub async fn admin_recent_plays(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.play_log.snapshot())
}

/// Measured-quality cache dump. One entry per `(stream_id, host)` key, each
/// with its raw sample buffer plus the aggregate the ranker actually reads.
/// No write endpoint — all writes come from the proxy's own sweep / per-play
/// hooks. Useful for sanity-checking what the ranker is using and for
/// finding 10-bit HEVC sources during pre-flight check 3.
pub async fn admin_measured_quality(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    #[derive(serde::Serialize)]
    struct Entry {
        stream_id: u64,
        host: String,
        samples: std::collections::VecDeque<crate::measured::Sample>,
        aggregate: Option<crate::measured::MeasuredQuality>,
    }
    let snap = state.measured.snapshot();
    let entries: Vec<Entry> = snap
        .into_iter()
        .map(|((stream_id, host), entry)| Entry {
            stream_id,
            host,
            aggregate: entry.aggregate(),
            samples: entry.samples,
        })
        .collect();
    Json(entries)
}

/// User-override (Step 9): the ranked candidate list `build_candidates` would
/// produce for this channel right now, enriched with per-row measured-quality,
/// cool-off step, and LKG age. The candidate-overlay UI consumes this to
/// show the user the rank order; OK on a row sends the URL back as
/// `?force_url=…` on a fresh play.
///
/// Read-only; no state mutation. Exposes raw upstream URLs — same risk
/// surface as `/admin/recent-plays` and `/admin/measured-quality`. This
/// inherits the project's single-tenant assumption (one user, deployed
/// behind a reverse-proxy on a private LAN). If that ever changes, this
/// endpoint plus the `/admin/*` endpoints all need an auth gate together.
pub async fn list_candidates(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Vec<CandidateDto>>, (StatusCode, String)> {
    let snap = state.catalog.snapshot();
    let channel = snap
        .lookup(&key)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, format!("unknown channel: {key}")))?;
    // Admin/candidate-overlay endpoint passes `None` for client_caps so it
    // shows the full ranked list (the caller is the user inspecting, not
    // a playback request the variant filter applies to).
    let candidates = crate::proxy::build_candidates(&state, &channel, None);
    let now = OffsetDateTime::now_utc();
    let dtos: Vec<CandidateDto> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let measured = state.measured.get(c.stream_id, &c.host);
            let cool_off_step = state.blacklist.cool_off_penalty(&c.url) as u8;
            let lkg_age_secs = state
                .blacklist
                .last_known_good_age(&channel.key, &c.url)
                .map(|d| d.as_secs());
            CandidateDto {
                url: c.url.clone(),
                host: c.host.clone(),
                stream_id: c.stream_id,
                rank_pos: i,
                cool_off_step,
                lkg_age_secs,
                measured,
            }
        })
        .collect();
    let _ = now; // reserved for future age fields
    Ok(Json(dtos))
}

#[derive(Debug, Serialize)]
pub struct CandidateDto {
    pub url: String,
    pub host: String,
    pub stream_id: u64,
    pub rank_pos: usize,
    pub cool_off_step: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lkg_age_secs: Option<u64>,
    /// Aggregate measured-quality for this `(stream_id, host)` pair. `None`
    /// when no samples have landed yet (sparse data — the candidate is
    /// still kept in the list, just unranked on the quality dimensions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<crate::measured::MeasuredQuality>,
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
///
/// Also waits briefly for the catalog to populate on cold-start so a probe
/// that races ahead of the first refresh doesn't 503 (which would cause the
/// client to drop the play-test cap and filter every channel of this kind
/// out of /api/channels for the session).
async fn probe_redirect(
    state: &AppState,
    headers: &HeaderMap,
    kind: ChannelKind,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // Closure used both as the wait predicate and the rank filter so they
    // can't drift. Mirrors the "probe target must be HLS" rule for radio.
    let probe_match = |c: &crate::canonical::CanonicalChannel| -> bool {
        c.kind == kind
            && match kind {
                // `live_audio_only_hls` is a play-test of HLS, so the
                // probe target must be a radio whose primary source is
                // HLS — not a raw MP3/AAC/Icecast (those play fine for
                // the user, but hls.js can't decode them and the probe
                // would report no audio-HLS capability).
                ChannelKind::Radio => matches!(
                    c.sources.iter().find_map(|s| s.radio_format),
                    Some(RadioFormat::Hls) | None,
                ),
                ChannelKind::Tv => true,
            }
    };

    // Boot race: on a cold start the client's cap probe can arrive before
    // the catalog loop's first refresh installs anything. 503ing immediately
    // makes the client drop `live_video_hls` (or `live_audio_only_hls`)
    // from X-Client-Caps for the session, which strips every channel of
    // that kind from /api/channels — the user sees an empty list with no
    // retry. Wait briefly for the catalog to populate; the client allows
    // ~12 s for this whole probe, so 8 s here leaves margin for the
    // downstream play attempt the redirect kicks off.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(8);
    let snap = loop {
        let snap = state.catalog.snapshot();
        if snap.channels.iter().any(&probe_match) {
            break snap;
        }
        if std::time::Instant::now() >= deadline {
            break snap;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    let curation = match kind {
        ChannelKind::Tv => &state.curation,
        ChannelKind::Radio => &state.radio_curation,
    };
    let alive: std::collections::HashSet<String> =
        state.hosts.alive_hosts_ranked().into_iter().collect();
    let mut ranked: Vec<&crate::canonical::CanonicalChannel> = snap
        .channels
        .iter()
        .filter(|c| probe_match(c))
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

/// Pick a homogeneous channel matching `target` (every measured source
/// agrees on the codec / pix_fmt / dvb_unsafe predicate) and 307-redirect
/// to `/play/<key>?probe=1`. Returns 404 when no matching channel exists —
/// the client interprets this as "this codec isn't probable here" and
/// drops the cap from its set.
///
/// Shares the cap-derivation predicate (`channel_matches_probe`) with
/// `caps_required` so the redirect target is consistent with what the
/// server's per-channel filter is using.
async fn probe_codec_redirect(
    state: &AppState,
    headers: &HeaderMap,
    target: crate::caps_cache::ProbeTarget,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let snap = state.catalog.snapshot();
    let alive_hosts = state.hosts.alive_hosts_ranked();
    let ch = crate::caps_cache::pick_probe_channel(
        &snap,
        &state.curation,
        &state.measured,
        &alive_hosts,
        target,
    )
    .ok_or((
        StatusCode::NOT_FOUND,
        format!("no channel matches probe target {:?}", target),
    ))?;

    let base = request_base_url(headers, state.config.public_base_url.as_deref());
    let url = format!(
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
        axum::http::HeaderValue::from_str(&url).map_err(|e| {
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

pub async fn probe_h264(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_codec_redirect(&state, &headers, crate::caps_cache::ProbeTarget::H264).await
}

pub async fn probe_hevc(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_codec_redirect(&state, &headers, crate::caps_cache::ProbeTarget::Hevc).await
}

pub async fn probe_hevc_main10(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_codec_redirect(&state, &headers, crate::caps_cache::ProbeTarget::HevcMain10).await
}

pub async fn probe_av1(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_codec_redirect(&state, &headers, crate::caps_cache::ProbeTarget::Av1).await
}

pub async fn probe_dvb_safe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    probe_codec_redirect(&state, &headers, crate::caps_cache::ProbeTarget::DvbSafe).await
}

#[derive(Debug, Serialize)]
pub struct ConditionalProbeDto {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Phase 3: conditional JSON probe endpoint for the `h264_excess_refs` cap.
/// Returns `{available, url}` where the URL is an absolute, probe-mode
/// /play link pinned to a specific variant via `probe_stream_id`. The
/// client `playProbe`s that URL; success → cap claimed; fail
/// (`x-fail-reason: probe-pin-failed`) → cap dropped. Always returns 200
/// — `{available:false}` when no qualifying variant exists right now (so
/// the JSON shape avoids the cross-origin opaque-redirect problem 307
/// would create).
pub async fn probe_h264_excess_refs_json(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<ConditionalProbeDto> {
    if !state.config.caps_v2_per_variant {
        return Json(ConditionalProbeDto { available: false, url: None });
    }
    let snap = state.catalog.snapshot();
    let alive_hosts = state.hosts.alive_hosts_ranked();
    let pick = crate::caps_cache::pick_probe_variant(
        &snap,
        &state.curation,
        &state.measured,
        &state.blacklist,
        &alive_hosts,
        state.config.caps_v2_stale_secs,
        "h264_excess_refs",
    );
    let Some((key, stream_id)) = pick else {
        return Json(ConditionalProbeDto { available: false, url: None });
    };
    let base = request_base_url(&headers, state.config.public_base_url.as_deref());
    let url = format!(
        "{}/play/{}.m3u8?probe=1&probe_stream_id={}",
        base.trim_end_matches('/'),
        key,
        stream_id,
    );
    Json(ConditionalProbeDto { available: true, url: Some(url) })
}

/// Phase 4: caps-readiness admin endpoint.
///
/// Per (stream_id, host) within the v2 emit scope, report the stability
/// state of every active cap tag plus sample-window stats. Operator
/// confirms `decisive_fraction == 1.0` before flipping
/// `caps_v2_per_variant`.
#[derive(Debug, Serialize)]
pub struct ReadinessEntry {
    pub stream_id: u64,
    pub host: String,
    pub channel_keys: Vec<String>,
    pub samples_count: usize,
    pub oldest_sample_age_secs: Option<i64>,
    pub newest_sample_age_secs: Option<i64>,
    pub states: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct CapsReadinessDto {
    pub decisive_fraction: f64,
    pub total_pairs: usize,
    pub decisive_pairs: usize,
    pub channels_hidden_by_caps: std::collections::BTreeMap<String, usize>,
    pub entries: Vec<ReadinessEntry>,
}

pub async fn admin_caps_readiness(
    State(state): State<Arc<AppState>>,
) -> Json<CapsReadinessDto> {
    let snap = state.catalog.snapshot();
    let alive_hosts = state.hosts.alive_hosts_ranked();
    let stale_secs = state.config.caps_v2_stale_secs;
    let now = time::OffsetDateTime::now_utc();
    let mut entries: Vec<ReadinessEntry> = Vec::new();
    let mut by_pair: std::collections::BTreeMap<(u64, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for ch in &snap.channels {
        if ch.kind == ChannelKind::Radio {
            continue;
        }
        for src in &ch.sources {
            if src.direct_source.is_some() {
                continue;
            }
            for host in &alive_hosts {
                if state.blacklist.is_host_bad(host) {
                    continue;
                }
                by_pair
                    .entry((src.stream_id, host.clone()))
                    .or_default()
                    .push(ch.key.clone());
            }
        }
    }
    let mut decisive_pairs = 0usize;
    for ((stream_id, host), channel_keys) in by_pair.into_iter() {
        let q = state.measured.get(stream_id, &host);
        let newest = state.measured.most_recent_at(stream_id, &host);
        let samples_count = q.as_ref().map(|x| x.samples_count).unwrap_or(0);
        let mut states: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let excess_state = q
            .as_ref()
            .map(|x| x.h264_excess_refs_state)
            .unwrap_or(crate::measured::ExcessRefsState::Unknown);
        let label = match excess_state {
            crate::measured::ExcessRefsState::On => "on",
            crate::measured::ExcessRefsState::Off => "off",
            crate::measured::ExcessRefsState::NotApplicable => "n/a",
            crate::measured::ExcessRefsState::Unknown => "unknown",
        };
        states.insert("h264_excess_refs".into(), label.into());
        if !matches!(excess_state, crate::measured::ExcessRefsState::Unknown) {
            decisive_pairs += 1;
        }
        let newest_age = newest.map(|at| (now - at).whole_seconds());
        // Oldest age — we don't track it explicitly; approximate via
        // sample-window position. The aggregate counts samples; we use
        // `samples_count * sweep_interval` as a rough lower bound. The
        // readiness UI just needs a "do we have data" signal.
        let oldest_age = newest_age;
        let _ = stale_secs;
        entries.push(ReadinessEntry {
            stream_id,
            host,
            channel_keys,
            samples_count,
            oldest_sample_age_secs: oldest_age,
            newest_sample_age_secs: newest_age,
            states,
        });
    }
    let total_pairs = entries.len();
    let decisive_fraction = if total_pairs == 0 {
        1.0
    } else {
        decisive_pairs as f64 / total_pairs as f64
    };
    let hidden = state.channels_hidden_by_caps.read().clone();
    Json(CapsReadinessDto {
        decisive_fraction,
        total_pairs,
        decisive_pairs,
        channels_hidden_by_caps: hidden,
        entries,
    })
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
    // index.html body is cached at startup (state.index_html). Avoids a
    // sync std::fs read on every request, which would block the runtime
    // thread for as long as the OS takes to satisfy the read.
    let body = state.index_html.as_str();
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
        // `defer` so the (~400 KB) hls.min.js download doesn't block the HTML
        // parser. The module script that imports it runs after DOMContentLoaded
        // anyway, and deferred classic scripts execute before module scripts,
        // so `window.Hls` is guaranteed defined by the time caps.js probes it.
        r#"<script src="lib/hls.min.js" defer></script>"#
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
