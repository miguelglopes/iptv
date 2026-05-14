use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Serialize;
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::canonical::{build_canonical, CanonicalChannel};
use crate::config::CatalogConfig;
use crate::default_order::Curation;
use crate::hosts::HostState;
use crate::xtream::{LiveStream, XtreamClient};

#[derive(Debug, Clone, Serialize)]
pub struct CatalogSnapshot {
    pub channels: Vec<CanonicalChannel>,
    pub by_key: HashMap<String, usize>,
    pub last_refreshed: Option<OffsetDateTime>,
    pub source_host: Option<String>,
    pub stream_count: usize,
}

impl CatalogSnapshot {
    pub fn empty() -> Self {
        Self {
            channels: Vec::new(),
            by_key: HashMap::new(),
            last_refreshed: None,
            source_host: None,
            stream_count: 0,
        }
    }

    pub fn lookup(&self, key: &str) -> Option<&CanonicalChannel> {
        self.by_key.get(key).and_then(|&i| self.channels.get(i))
    }
}

pub struct CatalogState {
    inner: RwLock<Arc<CatalogSnapshot>>,
    refresh_now: tokio::sync::Notify,
}

impl CatalogState {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Arc::new(CatalogSnapshot::empty())),
            refresh_now: tokio::sync::Notify::new(),
        }
    }

    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        Arc::clone(&self.inner.read())
    }

    pub fn request_refresh(&self) {
        self.refresh_now.notify_one();
    }

    fn install(&self, snap: CatalogSnapshot) {
        *self.inner.write() = Arc::new(snap);
    }
}

pub fn spawn_catalog_loop(
    state: Arc<CatalogState>,
    hosts_state: Arc<HostState>,
    client: XtreamClient,
    config: CatalogConfig,
    curation: Arc<Curation>,
) {
    tokio::spawn(async move {
        loop {
            let alive = hosts_state.alive_hosts_ranked();
            let mut last_refresh_succeeded = false;
            if alive.is_empty() {
                warn!("catalog refresh skipped: no alive hosts");
            } else {
                match refresh_once(&client, &alive).await {
                    Ok((streams, host)) => {
                        let channels = build_canonical(&streams, &curation);
                        let mut by_key = HashMap::with_capacity(channels.len());
                        for (i, c) in channels.iter().enumerate() {
                            by_key.insert(c.key.clone(), i);
                        }
                        let snap = CatalogSnapshot {
                            channels,
                            by_key,
                            last_refreshed: Some(OffsetDateTime::now_utc()),
                            source_host: Some(host),
                            stream_count: streams.len(),
                        };
                        info!(
                            "catalog refreshed: {} streams → {} canonical channels",
                            snap.stream_count,
                            snap.channels.len()
                        );
                        state.install(snap);
                        last_refresh_succeeded = true;
                    }
                    Err(e) => warn!("catalog refresh failed across all alive hosts: {e}"),
                }
            }

            let next_delay = if last_refresh_succeeded {
                Duration::from_secs(config.refresh_interval_secs)
            } else {
                Duration::from_secs(10)
            };

            tokio::select! {
                _ = tokio::time::sleep(next_delay) => {},
                _ = state.refresh_now.notified() => {},
            }
        }
    });
}

async fn refresh_once(
    client: &XtreamClient,
    alive: &[String],
) -> anyhow::Result<(Vec<LiveStream>, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    for h in alive {
        match client.all_live_streams(h).await {
            Ok(streams) => return Ok((streams, h.clone())),
            Err(e) => {
                warn!("get_live_streams failed via {}: {}", h, e);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no hosts to try")))
}
