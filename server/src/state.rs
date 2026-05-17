use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use dashmap::DashMap;
use reqwest::Client;
use std::time::Duration;

use crate::blacklist::Blacklist;
use crate::caps_cache::CapsRequiredCache;
use crate::catalog::CatalogState;
use crate::codec::StreamClassifier;
use crate::config::Config;
use crate::default_order::Curation;
use crate::epg::EpgState;
use crate::hosts::HostState;
use crate::measured::{MeasuredStore, PerPlayAccumulator};
use crate::play_log::PlayLog;
use crate::play_sessions::PlaySessions;
use crate::xtream::XtreamClient;

pub struct AppState {
    pub config: Arc<Config>,
    pub curation: Arc<Curation>,
    /// Second Curation instance from `[radio_curation]`. Same type, same
    /// schema — independent namespace so radio names don't collide with TV
    /// curation aliases. Looked up by `kind` in `api::list_channels`.
    pub radio_curation: Arc<Curation>,
    pub xtream: XtreamClient,
    pub hosts: Arc<HostState>,
    pub catalog: Arc<CatalogState>,
    pub epg: Arc<EpgState>,
    pub blacklist: Arc<Blacklist>,
    pub classifier: Arc<StreamClassifier>,
    pub upstream_http: Client,
    pub play_log: Arc<PlayLog>,
    /// Per-play upstream attribution. Lets `/api/feedback` blame the exact
    /// upstream the failing client was served, instead of racing against the
    /// global last-known-good. See `play_sessions.rs`.
    pub play_sessions: Arc<PlaySessions>,
    /// Measured stream-quality cache (per `(stream_id, host)` rolling buffer).
    pub measured: Arc<MeasuredStore>,
    /// In-progress per-play observations; committed as a single Sample when
    /// activity quiesces. Drained by a background task spawned from main.rs.
    pub per_play: Arc<PerPlayAccumulator>,
    /// Number of `/play/*` requests currently in flight. The measurement
    /// sweep yields when this is non-zero so it doesn't compete with users
    /// for upstream connection slots.
    pub active_plays: Arc<AtomicUsize>,
    /// Provider `max_connections` discovered from the first successful
    /// `authenticate()`. 0 means "not yet discovered" — sweep falls back
    /// to a conservative default until set.
    pub max_connections: Arc<AtomicU32>,
    /// `index.html` body read once at startup. Avoids a sync `std::fs::read_to_string`
    /// on every `/` and `/index.html` hit (which would block the tokio runtime
    /// thread). The file is static for the lifetime of the process; a server
    /// restart picks up edits.
    pub index_html: Arc<String>,
    /// Per-URL one-hop indirection cache for `.pls`/`.m3u` radio sources.
    /// Map `pls_url → (resolved_audio_url, resolved_at)`. TTL enforced at
    /// lookup time (1 h). Memory bounded by entry count (a few dozen).
    pub playlist_resolver_cache: Arc<DashMap<String, (String, Instant)>>,
    /// Phase 6: per-channel `caps_required` cache + cap-matrix version
    /// digest (X-Caps-Matrix-Version header). Lazy-rebuild on catalog
    /// refresh, measured generation bump, or alive-hosts change.
    pub caps_cache: Arc<CapsRequiredCache>,
    /// Phase 4 counter: cap tag → channels-hidden-this-request. Updated
    /// by `/api/channels` per its filter pass; surfaced in
    /// `/admin/caps-readiness`. Bounded cardinality (cap tag set).
    pub channels_hidden_by_caps:
        Arc<parking_lot::RwLock<std::collections::BTreeMap<String, usize>>>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        let xtream = XtreamClient::new(
            config.xtream.username.clone(),
            config.xtream.password.clone(),
            Duration::from_secs(8),
        )?;
        let curation = Arc::new(Curation::from_config(&config.curation)?);
        let radio_curation = Arc::new(Curation::from_config(&config.radio_curation)?);
        let hosts = Arc::new(HostState::new(&config.xtream.hosts));
        let catalog = Arc::new(CatalogState::new());
        let epg = Arc::new(EpgState::new(config.epg.clone()));
        // Blacklist (cool-off state machine) persists to data/blacklist.json
        // alongside the measured-quality cache. Same atomic-rename flush task
        // pattern; same docker-compose mount.
        let blacklist_path = std::path::PathBuf::from("data/blacklist.json");
        let blacklist = Arc::new(Blacklist::load_or_empty(
            config.blacklist.clone(),
            blacklist_path,
        ));
        let classifier = Arc::new(StreamClassifier::new());

        let upstream_http = Client::builder()
            .timeout(Duration::from_secs(config.proxy.upstream_timeout_secs))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::limited(8))
            .user_agent("iptv-proxy/0.1")
            .pool_idle_timeout(Duration::from_secs(30))
            .build()?;

        // Measured-quality store. Path is relative to the server CWD by
        // default (same convention as radios.m3u). docker-compose mounts
        // `./server/data` into the container so this survives rebuilds.
        let measured_path = std::path::PathBuf::from("data/measured_quality.json");
        let measured = Arc::new(MeasuredStore::load_or_empty(measured_path));

        // Cache index.html at startup so serve_index never has to do disk I/O
        // on the request path.
        let index_path = config.ui_dir.join("index.html");
        let index_html = Arc::new(std::fs::read_to_string(&index_path).with_context(|| {
            format!("reading index.html from {}", index_path.display())
        })?);

        Ok(Arc::new(Self {
            config: Arc::new(config),
            curation,
            radio_curation,
            xtream,
            hosts,
            catalog,
            epg,
            blacklist,
            classifier,
            upstream_http,
            play_log: Arc::new(PlayLog::new()),
            play_sessions: Arc::new(PlaySessions::new()),
            measured,
            per_play: Arc::new(PerPlayAccumulator::new()),
            active_plays: Arc::new(AtomicUsize::new(0)),
            max_connections: Arc::new(AtomicU32::new(0)),
            index_html,
            playlist_resolver_cache: Arc::new(DashMap::new()),
            caps_cache: Arc::new(CapsRequiredCache::new()),
            channels_hidden_by_caps: Arc::new(parking_lot::RwLock::new(
                std::collections::BTreeMap::new(),
            )),
        }))
    }
}
