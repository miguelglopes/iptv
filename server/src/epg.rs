use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures_util::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use serde::Serialize;
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
    let result = walk_all_in_parallel(client, candidates, timeout, &epg.fetch_semaphore).await;

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

#[derive(Debug, Clone)]
pub struct EpgCandidate {
    pub stream_id: u64,
    pub host: String,
    /// Higher = walked sooner. Used to prioritise catch-up-supporting streams
    /// so their `has_archive` flags survive the first-good-enough abort.
    pub priority: u8,
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
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority));
    let priority_total = candidates.iter().filter(|c| c.priority > 0).count();

    let mut tasks = FuturesUnordered::new();
    for cand in candidates {
        let client = client.clone();
        let sem = Arc::clone(semaphore);
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            let programs = fetch_one(&client, &cand, timeout).await;
            (cand, programs)
        }));
    }

    let mut best = WalkOutcome::default();
    let mut best_score = 0i64;
    let mut best_priority: u8 = 0;
    let mut priority_remaining = priority_total;

    while let Some(joined) = tasks.next().await {
        let Ok((cand, programs)) = joined else { continue };
        if cand.priority > 0 {
            priority_remaining = priority_remaining.saturating_sub(1);
        }
        let (score, span, count) = score_epg(&programs);
        let prefer = (cand.priority, score) > (best_priority, best_score);
        if prefer {
            best_score = score;
            best_priority = cand.priority;
            best = WalkOutcome {
                programs,
                program_count: count,
                span_hours: span,
                stream_id: Some(cand.stream_id),
                host: Some(cand.host),
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

async fn fetch_one(client: &XtreamClient, cand: &EpgCandidate, timeout: Duration) -> Vec<EpgProgram> {
    let full = tokio::time::timeout(timeout, client.simple_data_table(&cand.host, cand.stream_id)).await;
    if let Ok(Ok(p)) = full {
        if !p.is_empty() {
            return p;
        }
    }
    let short = tokio::time::timeout(timeout, client.short_epg(&cand.host, cand.stream_id, Some(8))).await;
    match short {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            debug!("EPG fetch failed for stream {} on {}: {}", cand.stream_id, cand.host, e);
            Vec::new()
        }
        Err(_) => {
            debug!("EPG fetch timeout for stream {} on {}", cand.stream_id, cand.host);
            Vec::new()
        }
    }
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
