use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures_util::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::config::EpgConfig;
use crate::xtream::{EpgProgram, XtreamClient};

#[derive(Debug, Clone, Serialize)]
pub struct EpgMeta {
    pub program_count: usize,
    pub span_hours: i64,
    pub source_stream_id: Option<u64>,
    pub source_host: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CachedEpg {
    pub programs: Vec<EpgProgram>,
    pub fetched_at: Instant,
    pub meta: EpgMeta,
}

pub struct EpgState {
    cache: DashMap<String, CachedEpg>,
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    fetch_semaphore: Arc<Semaphore>,
    config: EpgConfig,
}

impl EpgState {
    pub fn new(config: EpgConfig) -> Self {
        Self {
            cache: DashMap::new(),
            inflight: Mutex::new(HashMap::new()),
            fetch_semaphore: Arc::new(Semaphore::new(6)),
            config,
        }
    }

    pub fn get_fresh(&self, key: &str) -> Option<CachedEpg> {
        let entry = self.cache.get(key)?;
        if entry.fetched_at.elapsed() > Duration::from_secs(self.config.ttl_secs) {
            return None;
        }
        Some(entry.clone())
    }

    pub fn invalidate(&self, key: &str) {
        self.cache.remove(key);
    }

    pub fn known_keys(&self) -> Vec<String> {
        self.cache.iter().map(|e| e.key().clone()).collect()
    }
}

pub async fn fetch_epg_for_channel(
    epg: &EpgState,
    client: &XtreamClient,
    upstream_http: &Client,
    channel_key: &str,
    candidates: Vec<EpgCandidate>,
) -> CachedEpg {
    if let Some(cached) = epg.get_fresh(channel_key) {
        return cached;
    }

    let waiter = {
        let mut g = epg.inflight.lock();
        if let Some(notify) = g.get(channel_key) {
            Some(Arc::clone(notify))
        } else {
            let n = Arc::new(tokio::sync::Notify::new());
            g.insert(channel_key.to_string(), Arc::clone(&n));
            None
        }
    };

    if let Some(notify) = waiter {
        notify.notified().await;
        return epg
            .get_fresh(channel_key)
            .unwrap_or_else(|| empty_cached());
    }

    let timeout = Duration::from_secs(epg.config.fetch_timeout_secs);
    let result = walk_all_in_parallel(client, upstream_http, candidates, timeout, &epg.fetch_semaphore).await;

    let cached = CachedEpg {
        programs: dedupe_programs(result.programs),
        fetched_at: Instant::now(),
        meta: EpgMeta {
            program_count: result.program_count,
            span_hours: result.span_hours,
            source_stream_id: result.stream_id,
            source_host: result.host,
        },
    };
    epg.cache.insert(channel_key.to_string(), cached.clone());

    let notify = epg.inflight.lock().remove(channel_key);
    if let Some(n) = notify {
        n.notify_waiters();
    }

    cached
}

fn empty_cached() -> CachedEpg {
    CachedEpg {
        programs: Vec::new(),
        fetched_at: Instant::now(),
        meta: EpgMeta {
            program_count: 0,
            span_hours: 0,
            source_stream_id: None,
            source_host: None,
        },
    }
}

/// EPG candidate: either an Xtream source (host × stream_id pair, possibly one
/// of many for the same channel) or a self-contained RTP-radio fetch keyed by
/// the station's RTP channel code + a target date.
///
/// The walk-in-parallel / score / abort-early / dedupe / cache machinery is
/// shared — only `fetch_one` branches on the variant to pick the right HTTP
/// fetch + parser.
#[derive(Debug, Clone)]
pub enum EpgCandidate {
    Xtream {
        stream_id: u64,
        host: String,
        /// Higher = walked sooner. Used to prioritise catch-up-supporting
        /// streams so their `has_archive` flags survive the first-good-enough
        /// abort.
        priority: u8,
    },
    RtpRadio {
        /// RTP's per-station integer code in the URL pattern
        /// `https://www.rtp.pt/EPG/json/rtp-channels-page/list-grid/radio/{code}/{date}/lis`.
        code: u32,
        /// Target date in `D-M-YYYY` format (no zero-padding) — RTP's
        /// endpoint is picky about this. Build with `format_rtp_date()`.
        date: String,
    },
}

impl EpgCandidate {
    fn priority(&self) -> u8 {
        match self {
            EpgCandidate::Xtream { priority, .. } => *priority,
            // RtpRadio has no notion of catch-up so leave at 0 — walks
            // alongside non-archive Xtream candidates.
            EpgCandidate::RtpRadio { .. } => 0,
        }
    }
    fn stream_id_for_log(&self) -> u64 {
        match self {
            EpgCandidate::Xtream { stream_id, .. } => *stream_id,
            EpgCandidate::RtpRadio { code, .. } => u64::from(*code),
        }
    }
    fn host_for_log(&self) -> String {
        match self {
            EpgCandidate::Xtream { host, .. } => host.clone(),
            EpgCandidate::RtpRadio { code, date } => format!("rtp-radio/{code}/{date}"),
        }
    }
}

/// Format a date for RTP's `D-M-YYYY` (e.g. `14-5-2026`) endpoint path.
pub fn format_rtp_date(d: time::Date) -> String {
    format!(
        "{}-{}-{}",
        d.day(),
        u8::from(d.month()),
        d.year(),
    )
}

const GOOD_ENOUGH_PROGRAMS: usize = 20;
const GOOD_ENOUGH_SPAN_HOURS: i64 = 24;

#[derive(Default)]
struct WalkOutcome {
    programs: Vec<EpgProgram>,
    program_count: usize,
    span_hours: i64,
    stream_id: Option<u64>,
    host: Option<String>,
}

async fn walk_all_in_parallel(
    client: &XtreamClient,
    upstream_http: &Client,
    mut candidates: Vec<EpgCandidate>,
    timeout: Duration,
    semaphore: &Arc<Semaphore>,
) -> WalkOutcome {
    if candidates.is_empty() {
        return WalkOutcome::default();
    }

    // Walk high-priority candidates first so their response is one of the first
    // few to arrive — important for catch-up channels where only the archive
    // source's response carries `has_archive` flags.
    candidates.sort_by(|a, b| b.priority().cmp(&a.priority()));
    let priority_total = candidates.iter().filter(|c| c.priority() > 0).count();

    let mut tasks = FuturesUnordered::new();
    for cand in candidates {
        let client = client.clone();
        let http = upstream_http.clone();
        let sem = Arc::clone(semaphore);
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            let programs = fetch_one(&client, &http, &cand, timeout).await;
            (cand, programs)
        }));
    }

    let mut best = WalkOutcome::default();
    let mut best_score = 0i64;
    let mut best_priority: u8 = 0;
    let mut priority_remaining = priority_total;

    while let Some(joined) = tasks.next().await {
        let Ok((cand, programs)) = joined else { continue };
        let cand_priority = cand.priority();
        if cand_priority > 0 {
            priority_remaining = priority_remaining.saturating_sub(1);
        }
        let (score, span, count) = score_epg(&programs);
        let prefer = (cand_priority, score) > (best_priority, best_score);
        if prefer {
            best_score = score;
            best_priority = cand_priority;
            best = WalkOutcome {
                programs,
                program_count: count,
                span_hours: span,
                stream_id: Some(cand.stream_id_for_log()),
                host: Some(cand.host_for_log()),
            };
        }
        // Don't abort while we're still waiting on priority candidates — their
        // has_archive flags are the whole reason to prefer them.
        let priority_satisfied = best_priority > 0 || priority_remaining == 0;
        if priority_satisfied
            && best.program_count >= GOOD_ENOUGH_PROGRAMS
            && best.span_hours >= GOOD_ENOUGH_SPAN_HOURS
        {
            for t in tasks.iter() {
                t.abort();
            }
            break;
        }
    }

    if best.programs.is_empty() {
        warn!("EPG walk found no programs across all candidates");
    } else {
        debug!(
            "EPG walk picked stream {} on {} ({} programs, {}h span)",
            best.stream_id.unwrap_or(0),
            best.host.as_deref().unwrap_or("?"),
            best.program_count,
            best.span_hours,
        );
    }
    best
}

async fn fetch_one(
    client: &XtreamClient,
    upstream_http: &Client,
    cand: &EpgCandidate,
    timeout: Duration,
) -> Vec<EpgProgram> {
    match cand {
        EpgCandidate::Xtream { stream_id, host, .. } => {
            let full = tokio::time::timeout(timeout, client.simple_data_table(host, *stream_id)).await;
            if let Ok(Ok(p)) = full {
                if !p.is_empty() {
                    return p;
                }
            }
            let short = tokio::time::timeout(timeout, client.short_epg(host, *stream_id, Some(8))).await;
            match short {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => {
                    debug!("EPG fetch failed for stream {} on {}: {}", stream_id, host, e);
                    Vec::new()
                }
                Err(_) => {
                    debug!("EPG fetch timeout for stream {} on {}", stream_id, host);
                    Vec::new()
                }
            }
        }
        EpgCandidate::RtpRadio { code, date } => {
            match tokio::time::timeout(
                timeout,
                fetch_rtp_radio_day(upstream_http, *code, date),
            )
            .await
            {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => {
                    debug!("RTP radio EPG fetch failed for code {} ({}): {}", code, date, e);
                    Vec::new()
                }
                Err(_) => {
                    debug!("RTP radio EPG fetch timeout for code {} ({})", code, date);
                    Vec::new()
                }
            }
        }
    }
}

/// Fetch RTP radio EPG for one (code, date) pair. The endpoint pattern is
/// the same one iptv-org/epg uses for RTP TV — just with `radio` in the path
/// instead of `tv` — and returns morning/afternoon/evening buckets of programs.
async fn fetch_rtp_radio_day(
    http: &Client,
    code: u32,
    date: &str,
) -> anyhow::Result<Vec<EpgProgram>> {
    let url = format!(
        "https://www.rtp.pt/EPG/json/rtp-channels-page/list-grid/radio/{code}/{date}/lis"
    );
    let body = http.get(&url).send().await?.error_for_status()?.text().await?;
    Ok(parse_rtp_radio_response(&body))
}

fn parse_rtp_radio_response(body: &str) -> Vec<EpgProgram> {
    let value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let result = match value.get("result") {
        Some(Value::Object(m)) => m,
        _ => return Vec::new(),
    };
    // Buckets are "morning"/"afternoon"/"evening" — concat them in chronological order.
    let mut entries: Vec<&Value> = Vec::new();
    for bucket in ["morning", "afternoon", "evening"] {
        if let Some(Value::Array(arr)) = result.get(bucket) {
            entries.extend(arr.iter());
        }
    }
    let mut programs: Vec<EpgProgram> = Vec::new();
    for entry in entries {
        let Some(start) = parse_rtp_date_str(entry.get("date").and_then(|d| d.as_str())) else {
            continue;
        };
        let raw_name = entry.get("name").and_then(|d| d.as_str()).unwrap_or("");
        let raw_desc = entry.get("description").and_then(|d| d.as_str()).unwrap_or("");
        programs.push(EpgProgram {
            title: rtp_decode(raw_name),
            description: rtp_decode(raw_desc),
            start: Some(start),
            end: None, // RTP doesn't expose an explicit end; filled below from next start.
            has_archive: false,
        });
    }
    // Backfill `end` from the next program's start. The final program's end is
    // left None — the client UI handles None gracefully.
    for i in 0..programs.len() {
        if let Some(next_start) = programs.get(i + 1).and_then(|p| p.start) {
            programs[i].end = Some(next_start);
        }
    }
    programs
}

fn parse_rtp_date_str(s: Option<&str>) -> Option<OffsetDateTime> {
    let s = s?;
    // Format: "2026-05-14 00:00:00", interpreted in Europe/Lisbon. RTP's data
    // is given in local Lisbon time; we convert to UTC for consistency with
    // the Xtream EPG path (Xtream returns Unix timestamps which we already
    // store as UTC).
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let naive = time::PrimitiveDateTime::parse(s, fmt).ok()?;
    // Lisbon is UTC+0 in winter, UTC+1 in summer (DST). Approximate with a
    // fixed +1 offset for May–October, +0 otherwise. The client renders local
    // time so a one-hour skew on the DST boundary is the worst case.
    let month: u8 = naive.month().into();
    let offset = if (4..=10).contains(&month) {
        time::UtcOffset::from_hms(1, 0, 0).ok()?
    } else {
        time::UtcOffset::UTC
    };
    Some(naive.assume_offset(offset).to_offset(time::UtcOffset::UTC))
}

/// RTP's JSON sometimes contains HTML-entity-encoded characters
/// (`&ccedil;` → ç, etc.) and occasionally Base64-encoded title strings.
/// For now we just decode common HTML entities; the strings we've seen are
/// plain text otherwise.
fn rtp_decode(s: &str) -> String {
    let s = s
        .replace("&ccedil;", "ç")
        .replace("&Ccedil;", "Ç")
        .replace("&atilde;", "ã")
        .replace("&Atilde;", "Ã")
        .replace("&otilde;", "õ")
        .replace("&Otilde;", "Õ")
        .replace("&aacute;", "á")
        .replace("&eacute;", "é")
        .replace("&iacute;", "í")
        .replace("&oacute;", "ó")
        .replace("&uacute;", "ú")
        .replace("&Aacute;", "Á")
        .replace("&Eacute;", "É")
        .replace("&Iacute;", "Í")
        .replace("&Oacute;", "Ó")
        .replace("&Uacute;", "Ú")
        .replace("&amp;", "&");
    s
}

fn score_epg(programs: &[EpgProgram]) -> (i64, i64, usize) {
    if programs.is_empty() {
        return (0, 0, 0);
    }
    let count = programs.len();
    let span_hours: i64 = {
        let mut starts: Vec<OffsetDateTime> = programs.iter().filter_map(|p| p.start).collect();
        if starts.is_empty() {
            0
        } else {
            starts.sort();
            let span = *starts.last().unwrap() - *starts.first().unwrap();
            span.whole_hours()
        }
    };
    let score = count as i64 + span_hours * 2;
    (score, span_hours, count)
}

pub fn dedupe_programs(mut programs: Vec<EpgProgram>) -> Vec<EpgProgram> {
    if programs.is_empty() {
        return programs;
    }
    programs.retain(|p| p.start.is_some() && p.end.is_some());
    programs.sort_by_key(|p| p.start.unwrap());

    let window_minutes = 30;
    let mut kept: Vec<EpgProgram> = Vec::with_capacity(programs.len());
    for p in programs.into_iter() {
        let title_key = p.title.trim().to_lowercase();
        let mut dup = false;
        for (j, k) in kept.iter_mut().enumerate().rev() {
            let dt = (p.start.unwrap() - k.start.unwrap()).whole_minutes().abs();
            if dt > window_minutes {
                break;
            }
            if k.title.trim().to_lowercase() == title_key {
                let p_len = p.end.unwrap() - p.start.unwrap();
                let k_len = k.end.unwrap() - k.start.unwrap();
                if p_len > k_len {
                    kept[j] = p.clone();
                }
                dup = true;
                break;
            }
        }
        if !dup {
            kept.push(p);
        }
    }
    kept
}
