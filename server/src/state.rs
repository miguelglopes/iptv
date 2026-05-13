use std::sync::Arc;

use reqwest::Client;
use std::time::Duration;

use crate::blacklist::Blacklist;
use crate::catalog::CatalogState;
use crate::codec::StreamClassifier;
use crate::config::Config;
use crate::epg::EpgState;
use crate::hosts::HostState;
use crate::xtream::XtreamClient;

pub struct AppState {
    pub config: Arc<Config>,
    pub xtream: XtreamClient,
    pub hosts: Arc<HostState>,
    pub catalog: Arc<CatalogState>,
    pub epg: Arc<EpgState>,
    pub blacklist: Arc<Blacklist>,
    pub classifier: Arc<StreamClassifier>,
    pub upstream_http: Client,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        let xtream = XtreamClient::new(
            config.xtream.username.clone(),
            config.xtream.password.clone(),
            Duration::from_secs(8),
        )?;
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

        Ok(Arc::new(Self {
            config: Arc::new(config),
            xtream,
            hosts,
            catalog,
            epg,
            blacklist,
            classifier,
            upstream_http,
        }))
    }
}
