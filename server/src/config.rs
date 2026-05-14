use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen_addr: String,
    // Optional fallback URL prefix for absolute links the server emits (play_url,
    // segment proxies). Normally the server derives this from each request's
    // X-Forwarded-Proto / X-Forwarded-Host / Host headers so the same instance
    // works on LAN IP, public IP, and reverse-proxy hostnames without per-host
    // tuning. The fallback is only used when no Host header is available (e.g.,
    // HTTP/1.0 clients).
    #[serde(default)]
    pub public_base_url: Option<String>,
    #[serde(default = "default_ui_dir")]
    pub ui_dir: PathBuf,
    pub xtream: XtreamConfig,
    pub probe: ProbeConfig,
    pub catalog: CatalogConfig,
    pub epg: EpgConfig,
    pub blacklist: BlacklistConfig,
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub curation: CurationConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CurationConfig {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub display_overrides: HashMap<String, String>,
    #[serde(default)]
    pub provider_boosts: Vec<ProviderBoost>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderBoost {
    pub pattern: String,
    pub score: i32,
}

fn default_ui_dir() -> PathBuf { PathBuf::from("../app") }

#[derive(Debug, Clone, Deserialize)]
pub struct XtreamConfig {
    pub username: String,
    pub password: String,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeConfig {
    pub interval_secs: u64,
    pub timeout_ms: u64,
    #[serde(default = "default_parallelism")]
    pub parallelism: usize,
}

fn default_parallelism() -> usize { 4 }

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogConfig {
    pub refresh_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EpgConfig {
    pub ttl_secs: u64,
    pub fetch_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BlacklistConfig {
    pub host_fail_threshold: usize,
    pub url_ttl_secs: u64,
    pub host_ttl_secs: u64,
    #[serde(default = "default_demote_ttl")]
    pub demote_ttl_secs: u64,
}

fn default_demote_ttl() -> u64 { 10800 }

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    pub upstream_timeout_secs: u64,
    #[serde(default = "default_segment_buffer")]
    pub segment_buffer_bytes: usize,
    #[serde(default = "default_play_budget")]
    pub play_budget_secs: u64,
    #[serde(default = "default_per_attempt")]
    pub per_attempt_timeout_secs: u64,
    #[serde(default = "default_validate_count")]
    pub opportunistic_validate_count: usize,
    #[serde(default = "default_validate_timeout")]
    pub opportunistic_validate_timeout_secs: u64,
}

fn default_segment_buffer() -> usize { 65536 }
fn default_play_budget() -> u64 { 10 }
fn default_per_attempt() -> u64 { 5 }
fn default_validate_count() -> usize { 2 }
fn default_validate_timeout() -> u64 { 4 }

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&body).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.xtream.hosts.is_empty(), "xtream.hosts cannot be empty");
        anyhow::ensure!(!self.xtream.username.is_empty(), "xtream.username cannot be empty");
        anyhow::ensure!(!self.xtream.password.is_empty(), "xtream.password cannot be empty");
        Ok(())
    }
}
