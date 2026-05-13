use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::config::ProbeConfig;
use crate::xtream::XtreamClient;

#[derive(Debug, Clone, Serialize)]
pub struct HostStatus {
    pub host: String,
    pub alive: bool,
    pub latency_ms: Option<u64>,
    pub last_probe: Option<OffsetDateTime>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct HostStateInner {
    pub statuses: Vec<HostStatus>,
}

pub struct HostState {
    inner: RwLock<HostStateInner>,
    reprobe: Notify,
}

impl HostState {
    pub fn new(hosts: &[String]) -> Self {
        let statuses = hosts
            .iter()
            .map(|h| HostStatus {
                host: h.clone(),
                alive: false,
                latency_ms: None,
                last_probe: None,
                error: None,
            })
            .collect();
        Self {
            inner: RwLock::new(HostStateInner { statuses }),
            reprobe: Notify::new(),
        }
    }

    pub fn snapshot(&self) -> Vec<HostStatus> {
        self.inner.read().statuses.clone()
    }

    pub fn alive_hosts_ranked(&self) -> Vec<String> {
        let mut alive: Vec<HostStatus> = self
            .inner
            .read()
            .statuses
            .iter()
            .filter(|s| s.alive)
            .cloned()
            .collect();
        alive.sort_by_key(|s| s.latency_ms.unwrap_or(u64::MAX));
        alive.into_iter().map(|s| s.host).collect()
    }

    pub fn request_reprobe(&self) {
        self.reprobe.notify_one();
    }

    fn update(&self, host: &str, result: ProbeResult) {
        let mut g = self.inner.write();
        if let Some(entry) = g.statuses.iter_mut().find(|s| s.host == host) {
            entry.alive = result.alive;
            entry.latency_ms = result.latency_ms;
            entry.last_probe = Some(OffsetDateTime::now_utc());
            entry.error = result.error;
        }
    }
}

#[derive(Debug)]
struct ProbeResult {
    alive: bool,
    latency_ms: Option<u64>,
    error: Option<String>,
}

pub fn spawn_probe_loop(
    state: Arc<HostState>,
    client: XtreamClient,
    config: ProbeConfig,
    hosts: Vec<String>,
) {
    tokio::spawn(async move {
        loop {
            run_one_round(&state, &client, &config, &hosts).await;
            let sleep = tokio::time::sleep(Duration::from_secs(config.interval_secs));
            tokio::select! {
                _ = sleep => {},
                _ = state.reprobe.notified() => {
                    debug!("reprobe requested, running round now");
                },
            }
        }
    });
}

async fn run_one_round(
    state: &Arc<HostState>,
    client: &XtreamClient,
    config: &ProbeConfig,
    hosts: &[String],
) {
    let timeout = Duration::from_millis(config.timeout_ms);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.parallelism.max(1)));

    let mut tasks = Vec::new();
    for host in hosts {
        let host = host.clone();
        let client = client.clone();
        let state = Arc::clone(state);
        let sem = Arc::clone(&semaphore);
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            let result = probe_one(&client, &host, timeout).await;
            state.update(&host, result);
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    let alive = state.alive_hosts_ranked();
    if alive.is_empty() {
        warn!("no alive hosts after probe round");
    } else {
        info!("probe round done: {} alive (top: {})", alive.len(), alive[0]);
    }
}

async fn probe_one(client: &XtreamClient, host: &str, timeout: Duration) -> ProbeResult {
    let t0 = Instant::now();
    let fut = client.authenticate(host);
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(info)) => {
            let latency = t0.elapsed().as_millis() as u64;
            if info.is_authenticated() {
                ProbeResult { alive: true, latency_ms: Some(latency), error: None }
            } else {
                ProbeResult { alive: false, latency_ms: Some(latency), error: Some("auth=0".into()) }
            }
        }
        Ok(Err(e)) => ProbeResult { alive: false, latency_ms: None, error: Some(e.to_string()) },
        Err(_) => ProbeResult { alive: false, latency_ms: None, error: Some("timeout".into()) },
    }
}
