use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Serialize;
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::canonical::{build_canonical, CanonicalChannel};
use crate::config::{CatalogConfig, RadioConfig};
use crate::default_order::Curation;
use crate::hosts::HostState;
use crate::radio::load_radio_streams;
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
    radio_curation: Arc<Curation>,
    radio_config: RadioConfig,
) {
    tokio::spawn(async move {
        loop {
            let alive = hosts_state.alive_hosts_ranked();
            let mut last_refresh_succeeded = false;
            // Load radio streams every refresh — local file read, ~ms. Keeps the
            // server picking up M3U edits without a restart. If [radio].enabled
            // is false or the file is missing, just emit zero radio streams and
            // continue (radio failures must never block the TV refresh).
            let radio_streams = if radio_config.enabled {
                match load_radio_streams(&radio_config.m3u_path) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "radio M3U load failed (catalog continues with TV only): {e}"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            if alive.is_empty() && radio_streams.is_empty() {
                warn!("catalog refresh skipped: no alive hosts and no radio sources");
            } else {
                // Fetch streams from EVERY alive host in parallel so per-host-
                // exclusive channels surface AND so each stream stays tagged
                // with the host that actually has it. build_candidates uses
                // origin_host to route the right stream_id to the right host
                // (instead of speculatively fanning across all hosts and 404-ing
                // when the provider doesn't share IDs).
                let (streams, succeeded_hosts) = refresh_all(&client, &alive).await;

                // Build TV and radio canonicals separately so each uses its
                // own curation (aliases / display_overrides / order).
                // build_canonical groups by canonical_key, so feeding both
                // into one call would let radio aliases pollute TV keys.
                let mut channels = build_canonical(&streams, &curation);
                let radio_channels = build_canonical(&radio_streams, &radio_curation);
                let radio_count = radio_channels.len();
                channels.extend(radio_channels);

                let mut by_key = HashMap::with_capacity(channels.len());
                for (i, c) in channels.iter().enumerate() {
                    by_key.insert(c.key.clone(), i);
                }
                let stream_count = streams.len() + radio_streams.len();
                let source_host = if succeeded_hosts.is_empty() {
                    None
                } else if succeeded_hosts.len() == 1 {
                    Some(succeeded_hosts[0].clone())
                } else {
                    Some(format!("{} hosts", succeeded_hosts.len()))
                };
                let snap = CatalogSnapshot {
                    channels,
                    by_key,
                    last_refreshed: Some(OffsetDateTime::now_utc()),
                    source_host,
                    stream_count,
                };
                info!(
                    "catalog refreshed: {} streams from {}/{} alive hosts → {} canonical channels ({} radio)",
                    snap.stream_count,
                    succeeded_hosts.len(),
                    alive.len(),
                    snap.channels.len(),
                    radio_count,
                );
                state.install(snap);
                // Only count as a full success when we actually got TV
                // streams. A radio-only "success" during the cold-start
                // window (before the probe loop has marked any host
                // alive) would otherwise sleep for refresh_interval_secs
                // and leave the user with no TV channels for an hour.
                last_refresh_succeeded = !streams.is_empty();
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

/// Fetch the live-stream list from every alive host concurrently. Returns the
/// union of streams (each already tagged with `origin_host` by
/// `XtreamClient::all_live_streams`) plus the set of hosts that responded
/// successfully. Per-host failures are logged but don't abort the merge —
/// a single dead host shouldn't strip other hosts' channels from the catalog.
async fn refresh_all(
    client: &XtreamClient,
    alive: &[String],
) -> (Vec<LiveStream>, Vec<String>) {
    use tokio::task::JoinSet;
    let mut set = JoinSet::new();
    for h in alive {
        let h = h.clone();
        let client = client.clone();
        set.spawn(async move {
            let res = client.all_live_streams(&h).await;
            (h, res)
        });
    }
    let mut all_streams: Vec<LiveStream> = Vec::new();
    let mut succeeded: Vec<String> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((host, Ok(streams))) => {
                all_streams.extend(streams);
                succeeded.push(host);
            }
            Ok((host, Err(e))) => {
                warn!("get_live_streams failed via {}: {}", host, e);
            }
            Err(e) => {
                warn!("catalog fetch task panicked: {e}");
            }
        }
    }
    (all_streams, succeeded)
}
