use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use time::OffsetDateTime;

use crate::canonical::quality_tier;
use crate::codec::Classification;
use crate::epg::{fetch_epg_for_channel, EpgCandidate};
use crate::state::AppState;
use crate::xtream::EpgProgram;

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
    type SortKey = (u8, usize, String, usize);
    let mut out: Vec<(SortKey, ChannelDto)> = snap
        .channels
        .iter()
        .enumerate()
        .map(|(orig_i, ch)| {
            let logo = ch.sources.iter().find_map(|s| s.logo.clone());
            let d = state.curation.rank_of(&ch.key);
            let bucket: u8 = if d.is_some() { 0 } else { 1 };
            let sub = d.unwrap_or(usize::MAX);
            let archive_src = ch.pick_archive_source();
            let tv_archive = archive_src.is_some();
            let tv_archive_duration = archive_src.and_then(|s| s.tv_archive_duration);
            let tv_archive_quality = archive_src.and_then(|s| quality_tier(&s.name));
            let dto = ChannelDto {
                key: ch.key.clone(),
                name: ch.name.clone(),
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
    let alive = state.hosts.alive_hosts_ranked();
    if alive.is_empty() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no alive hosts".into()));
    }
    let mut cands = Vec::new();
    for src in &ch.sources {
        let priority = if src.tv_archive { 1 } else { 0 };
        for host in &alive {
            if state.blacklist.is_host_bad(host) {
                continue;
            }
            cands.push(EpgCandidate {
                stream_id: src.stream_id,
                host: host.clone(),
                priority,
            });
        }
    }
    let cached = fetch_epg_for_channel(&state.epg, &state.xtream, &key, cands).await;
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
        .filter(|e| e.classification.unplayable_on_webos_b4())
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
    let blamed = state.blacklist.drop_last_known_good(&key);
    match body.kind {
        FeedbackKind::Fail => {
            if let Some(url) = blamed.as_deref() {
                state.blacklist.note_url_failed(url);
                tracing::info!(channel = %key, url = %url, error = %error, "client-reported failure: blacklisted current pick");
            } else {
                tracing::info!(channel = %key, error = %error, "client-reported failure: no current pick to blacklist");
            }
        }
        FeedbackKind::Demote => {
            if let Some(url) = blamed.as_deref() {
                state.blacklist.demote_url(url);
                tracing::info!(channel = %key, url = %url, error = %error, "client-reported demote: deprioritized current pick");
            } else {
                tracing::info!(channel = %key, error = %error, "client-reported demote: no current pick to demote");
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
