use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::Serialize;

use crate::config::BlacklistConfig;

#[derive(Debug, Clone, Serialize)]
pub struct UrlEntry {
    pub url: String,
    pub fails: u32,
    pub last_fail_secs_ago: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostFailEntry {
    pub host: String,
    pub distinct_streams_failed: usize,
    pub last_fail_secs_ago: u64,
}

#[derive(Debug, Default)]
struct Inner {
    failed_urls: HashMap<String, (u32, Instant)>,
    host_streams: HashMap<String, HashMap<String, Instant>>,
    last_known_good: HashMap<String, (String, Instant)>,
    demoted_urls: HashMap<String, (u32, Instant)>,
}

pub struct Blacklist {
    inner: RwLock<Inner>,
    config: BlacklistConfig,
}

impl Blacklist {
    pub fn new(config: BlacklistConfig) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            config,
        }
    }

    pub fn note_url_failed(&self, url: &str) {
        let mut g = self.inner.write();
        let now = Instant::now();
        let entry = g.failed_urls.entry(url.to_string()).or_insert((0, now));
        entry.0 += 1;
        entry.1 = now;

        if let (Some(host), Some(sid)) = (host_of(url), stream_id_of(url)) {
            g.host_streams
                .entry(host)
                .or_default()
                .insert(sid, now);
        }
    }

    pub fn note_url_succeeded(&self, channel_key: &str, url: &str) {
        let mut g = self.inner.write();
        g.last_known_good.insert(channel_key.to_string(), (url.to_string(), Instant::now()));
        g.failed_urls.remove(url);
    }

    pub fn last_known_good(&self, channel_key: &str) -> Option<String> {
        let g = self.inner.read();
        let (url, ts) = g.last_known_good.get(channel_key)?;
        if ts.elapsed() > Duration::from_secs(86400) {
            return None;
        }
        Some(url.clone())
    }

    pub fn drop_last_known_good(&self, channel_key: &str) -> Option<String> {
        let mut g = self.inner.write();
        g.last_known_good.remove(channel_key).map(|(u, _)| u)
    }

    pub fn is_url_failed(&self, url: &str) -> bool {
        let g = self.inner.read();
        let Some((_, ts)) = g.failed_urls.get(url) else { return false };
        ts.elapsed() < Duration::from_secs(self.config.url_ttl_secs)
    }

    pub fn is_host_bad(&self, host: &str) -> bool {
        let g = self.inner.read();
        let Some(streams) = g.host_streams.get(host) else { return false };
        let ttl = Duration::from_secs(self.config.host_ttl_secs);
        let recent = streams.values().filter(|t| t.elapsed() < ttl).count();
        recent >= self.config.host_fail_threshold
    }

    pub fn snapshot_urls(&self) -> Vec<UrlEntry> {
        let g = self.inner.read();
        let now = Instant::now();
        let ttl = Duration::from_secs(self.config.url_ttl_secs);
        g.failed_urls
            .iter()
            .filter(|(_, (_, t))| now.duration_since(*t) < ttl)
            .map(|(u, (n, t))| UrlEntry {
                url: u.clone(),
                fails: *n,
                last_fail_secs_ago: now.duration_since(*t).as_secs(),
            })
            .collect()
    }

    pub fn snapshot_hosts(&self) -> Vec<HostFailEntry> {
        let g = self.inner.read();
        let now = Instant::now();
        let ttl = Duration::from_secs(self.config.host_ttl_secs);
        g.host_streams
            .iter()
            .map(|(h, streams)| {
                let recent: Vec<_> = streams
                    .values()
                    .filter(|t| now.duration_since(**t) < ttl)
                    .collect();
                let last = recent.iter().max().copied().copied().unwrap_or(now);
                HostFailEntry {
                    host: h.clone(),
                    distinct_streams_failed: recent.len(),
                    last_fail_secs_ago: now.duration_since(last).as_secs(),
                }
            })
            .filter(|e| e.distinct_streams_failed > 0)
            .collect()
    }

    pub fn clear_blacklist(&self) {
        let mut g = self.inner.write();
        g.failed_urls.clear();
        g.host_streams.clear();
    }

    pub fn clear_demoted(&self) {
        let mut g = self.inner.write();
        g.demoted_urls.clear();
    }

    pub fn clear_all(&self) {
        let mut g = self.inner.write();
        g.failed_urls.clear();
        g.host_streams.clear();
        g.demoted_urls.clear();
        g.last_known_good.clear();
    }

    pub fn demote_url(&self, url: &str) {
        let mut g = self.inner.write();
        let now = Instant::now();
        let entry = g.demoted_urls.entry(url.to_string()).or_insert((0, now));
        entry.0 += 1;
        entry.1 = now;
    }

    pub fn is_url_demoted(&self, url: &str) -> bool {
        let g = self.inner.read();
        let Some((_, ts)) = g.demoted_urls.get(url) else { return false };
        ts.elapsed() < Duration::from_secs(self.config.demote_ttl_secs)
    }

    pub fn snapshot_demoted(&self) -> Vec<UrlEntry> {
        let g = self.inner.read();
        let now = Instant::now();
        let ttl = Duration::from_secs(self.config.demote_ttl_secs);
        g.demoted_urls
            .iter()
            .filter(|(_, (_, t))| now.duration_since(*t) < ttl)
            .map(|(u, (n, t))| UrlEntry {
                url: u.clone(),
                fails: *n,
                last_fail_secs_ago: now.duration_since(*t).as_secs(),
            })
            .collect()
    }
}

fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let host = after_scheme.split('/').next()?;
    Some(format!("{}://{}", url.split_once("://")?.0, host))
}

fn stream_id_of(url: &str) -> Option<String> {
    let after_live = url.split("/live/").nth(1)?;
    let parts: Vec<&str> = after_live.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let last = parts[2];
    let id = last.split('.').next()?;
    Some(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_parts() {
        let u = "http://cf.8kgaminghub.shop/live/USER/PASS/12345.m3u8";
        assert_eq!(host_of(u).unwrap(), "http://cf.8kgaminghub.shop");
        assert_eq!(stream_id_of(u).unwrap(), "12345");
    }
}
