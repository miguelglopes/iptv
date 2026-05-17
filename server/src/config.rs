use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Optional radio block. When present + enabled, the server parses the
    /// referenced M3U at every catalog refresh and merges the resulting
    /// `kind = Radio` streams into the canonical channel list.
    #[serde(default)]
    pub radio: RadioConfig,
    /// Curation block for radio channels. Independent from `[curation]` so
    /// the radio order / aliases / display_overrides don't have to share a
    /// namespace with TV. Reuses the same `CurationConfig` type — no schema
    /// fork.
    #[serde(default)]
    pub radio_curation: CurationConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RadioConfig {
    /// If false (default), no radio is loaded and the kind=Radio channel
    /// list is empty. Lets us land the code with the feature dark.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the vendored M3U file, relative to the server binary's CWD.
    /// Defaults to `radios.m3u` next to `config.toml`.
    #[serde(default = "default_radio_m3u_path")]
    pub m3u_path: PathBuf,
}

fn default_radio_m3u_path() -> PathBuf { PathBuf::from("radios.m3u") }

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CurationConfig {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub display_overrides: HashMap<String, String>,
    /// Per-channel-key logo URL override. Wins over any source-derived logo,
    /// for cases where upstream stream_icon is wrong (e.g. one channel's logo
    /// is mistakenly another channel's image) and uniqueness filtering can't
    /// help (the wrong URL is unique to this channel).
    #[serde(default)]
    pub logo_overrides: HashMap<String, String>,
    #[serde(default)]
    pub provider_boosts: Vec<ProviderBoost>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBoost {
    pub pattern: String,
    pub score: i32,
}

fn default_ui_dir() -> PathBuf { PathBuf::from("../app") }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XtreamConfig {
    pub username: String,
    pub password: String,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    pub interval_secs: u64,
    pub timeout_ms: u64,
    #[serde(default = "default_parallelism")]
    pub parallelism: usize,
    /// Freshness loop (Step 6): runs in the background after the bootstrap
    /// sweep and keeps the measured-quality cache warm by re-probing keys
    /// older than `freshness_ttl_secs`.
    ///
    /// - `None` (default) — auto-gated by `max_connections`: OFF when
    ///   `max_cons ≤ 2`, ON at 900 s (15 min) when `max_cons ≥ 3`.
    /// - `Some(0)` — force-disable (incident-response escape hatch).
    /// - `Some(n>0)` — force-enable at `n` seconds regardless of `max_cons`.
    ///
    /// `Option` shape avoids the magic-u32-sentinel footgun where 0 means
    /// disabled / -1 means "auto" / etc.
    #[serde(default)]
    pub freshness_loop_interval_secs: Option<u64>,
    /// Re-probe threshold (seconds). Samples older than this are eligible
    /// for re-probe in the freshness loop. Default 3600 (1 h).
    #[serde(default = "default_freshness_ttl")]
    pub freshness_ttl_secs: u64,
}

fn default_parallelism() -> usize { 4 }
fn default_freshness_ttl() -> u64 { 3600 }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogConfig {
    pub refresh_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpgConfig {
    pub ttl_secs: u64,
    pub fetch_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlacklistConfig {
    pub host_fail_threshold: usize,
    pub host_ttl_secs: u64,
    /// Cool-off durations per step (seconds). Step 0 = no cool-off; steps
    /// 1..=4 map to entries 0..=3 of this array. Defaults to
    /// `[60, 300, 1800, 21600]` → 1 min / 5 min / 30 min / 6 h. Each
    /// failure bumps the step by one; clean-play heartbeats reset to 0.
    /// Currently only documented (the rank-tuple uses the step number,
    /// not the duration); Phase 6's freshness loop will consume these
    /// values to schedule re-probes against URLs in cool-off.
    #[serde(default = "default_cool_off_steps")]
    #[allow(dead_code)]
    pub cool_off_steps_secs: [u64; 4],
    /// Window during which the previous heartbeat is still considered
    /// "fresh" — tolerates one missed tick at the 30 s client cadence.
    /// Default 60 s.
    #[serde(default = "default_heartbeat_window")]
    pub heartbeat_window_secs: u64,
    /// Minimum time since the URL's last error before a heartbeat-driven
    /// cool-off reset can fire. Default 300 s (5 min).
    #[serde(default = "default_clean_play_reset")]
    pub clean_play_reset_secs: u64,
}

fn default_cool_off_steps() -> [u64; 4] { [60, 300, 1800, 21600] }
fn default_heartbeat_window() -> u64 { 60 }
fn default_clean_play_reset() -> u64 { 300 }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub upstream_timeout_secs: u64,
    /// Reserved for a future per-segment buffer-tuning consumer. Kept in
    /// the config so on-disk TOML stays stable while we decide whether to
    /// wire it up. Annotated `dead_code` so the clippy gate stays green.
    #[serde(default = "default_segment_buffer")]
    #[allow(dead_code)]
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
// Generous enough to exhaust ~12 attempts at the default per-attempt cap.
// Practical candidate counts (alive hosts × variants × demoted bucket) sit well
// below this; the budget is just a final safety net against pathological loops.
fn default_play_budget() -> u64 { 60 }
fn default_per_attempt() -> u64 { 5 }
fn default_validate_count() -> usize { 2 }
// Match `per_attempt_timeout_secs` plus a small margin: validation runs in
// background and we don't want it to flag slow-but-working URLs that an actual
// play would still tolerate.
fn default_validate_timeout() -> u64 { 6 }

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
