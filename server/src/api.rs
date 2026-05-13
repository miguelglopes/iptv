use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use time::OffsetDateTime;

use crate::canonical::quality_tier;
use crate::codec::Classification;
use crate::default_order;
use crate::epg::{fetch_epg_for_channel, EpgCandidate};
use crate::state::AppState;
use crate::xtream::EpgProgram;

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

pub async fn list_channels(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snap = state.catalog.snapshot();
    let base = &state.config.public_base_url;
    type SortKey = (u8, usize, String, usize);
    let mut out: Vec<(SortKey, ChannelDto)> = snap
        .channels
        .iter()
        .enumerate()
        .map(|(orig_i, ch)| {
            let logo = ch.sources.iter().find_map(|s| s.logo.clone());
            let d = default_order::rank(&ch.key);
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
    StatusCode::NO_CONTENT
}
