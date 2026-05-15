use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use reqwest::Client;
use std::time::Duration;

use crate::blacklist::Blacklist;
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
        let blacklist = Arc::new(Blacklist::new(config.blacklist.clone()));
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
        }))
    }
}
