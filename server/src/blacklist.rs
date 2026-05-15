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
        let window = Duration::from_secs(self.config.url_fail_window_secs);
        let entry = g.failed_urls.entry(url.to_string()).or_insert((0, now));
        // Reset the counter when the previous fail is older than the rolling
        // window. Without this, an old single fail + a fresh one would jump
        // the count from 1 to 2 across a stale gap — defeating the point of
        // a windowed threshold.
        if entry.0 > 0 && entry.1.elapsed() > window {
            entry.0 = 1;
        } else {
            entry.0 += 1;
        }
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

    /// Drop the channel's last-known-good only if it equals `url`. Used by the
    /// feedback path when we know the exact upstream the client was playing
    /// (via play-id lookup) — we don't want to evict another client's LKG by
    /// accident.
    pub fn drop_last_known_good_if_matches(&self, channel_key: &str, url: &str) -> bool {
        let mut g = self.inner.write();
        let matches = g
            .last_known_good
            .get(channel_key)
            .map(|(u, _)| u == url)
            .unwrap_or(false);
        if matches {
            g.last_known_good.remove(channel_key);
        }
        matches
    }

    pub fn is_url_failed(&self, url: &str) -> bool {
        let g = self.inner.read();
        let Some((count, ts)) = g.failed_urls.get(url) else { return false };
        // Threshold-gated. First N-1 failures only record the count + recency
        // (and the caller may demote the URL separately). At the Nth failure
        // within the window, the URL is treated as failed until the TTL
        // elapses. Tolerates network blips, slow-Wi-Fi false-positives, and
        // occasional cross-client misblame.
        *count >= self.config.url_fail_threshold
            && ts.elapsed() < Duration::from_secs(self.config.url_ttl_secs)
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

    /// Record a failure AND demote in one call. Use this for every observed
    /// failure (proxy-side errors, timeouts, validation failures, client
    /// feedback) so the URL is deprioritized immediately while the windowed
    /// fail-count walks toward the blacklist threshold.
    pub fn mark_failed(&self, url: &str) {
        self.note_url_failed(url);
        self.demote_url(url);
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
    use crate::config::BlacklistConfig;

    fn cfg(url_fail_threshold: u32) -> BlacklistConfig {
        BlacklistConfig {
            host_fail_threshold: 4,
            url_ttl_secs: 3600,
            host_ttl_secs: 3600,
            demote_ttl_secs: 10800,
            url_fail_threshold,
            url_fail_window_secs: 300,
        }
    }

    #[test]
    fn parses_url_parts() {
        let u = "http://cf.8kgaminghub.shop/live/USER/PASS/12345.m3u8";
        assert_eq!(host_of(u).unwrap(), "http://cf.8kgaminghub.shop");
        assert_eq!(stream_id_of(u).unwrap(), "12345");
    }

    #[test]
    fn url_not_failed_below_threshold() {
        let bl = Blacklist::new(cfg(3));
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_url_failed(u);
        assert!(!bl.is_url_failed(u), "1 fail < threshold");
        bl.note_url_failed(u);
        assert!(!bl.is_url_failed(u), "2 fails < threshold");
    }

    #[test]
    fn url_failed_at_threshold() {
        let bl = Blacklist::new(cfg(3));
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_url_failed(u);
        bl.note_url_failed(u);
        bl.note_url_failed(u);
        assert!(bl.is_url_failed(u), "3rd fail crosses threshold");
    }

    #[test]
    fn threshold_of_one_blacklists_immediately() {
        // Defensive: a config that sets threshold=1 mirrors the pre-threshold
        // behaviour (any fail blacklists).
        let bl = Blacklist::new(cfg(1));
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_url_failed(u);
        assert!(bl.is_url_failed(u));
    }

    #[test]
    fn note_success_clears_fail_count() {
        let bl = Blacklist::new(cfg(3));
        let u = "http://h/live/U/P/1.m3u8";
        bl.note_url_failed(u);
        bl.note_url_failed(u);
        bl.note_url_succeeded("chan", u);
        // The same URL should now be tracked as LKG and removed from failed_urls.
        assert_eq!(bl.last_known_good("chan").as_deref(), Some(u));
        assert!(!bl.is_url_failed(u));
        // A fresh failure starts counting from zero again, not from 2.
        bl.note_url_failed(u);
        assert!(!bl.is_url_failed(u), "after success, count restarts");
    }

    #[test]
    fn mark_failed_demotes_and_counts() {
        let bl = Blacklist::new(cfg(3));
        let u = "http://h/live/U/P/1.m3u8";
        bl.mark_failed(u);
        assert!(bl.is_url_demoted(u), "mark_failed demotes too");
        assert!(!bl.is_url_failed(u), "first mark below threshold");
        bl.mark_failed(u);
        bl.mark_failed(u);
        assert!(bl.is_url_failed(u), "third mark crosses threshold");
    }

    #[test]
    fn drop_last_known_good_if_matches_only_when_url_matches() {
        let bl = Blacklist::new(cfg(3));
        bl.note_url_succeeded("rtp1", "http://a");
        assert!(!bl.drop_last_known_good_if_matches("rtp1", "http://b"));
        assert_eq!(bl.last_known_good("rtp1").as_deref(), Some("http://a"));
        assert!(bl.drop_last_known_good_if_matches("rtp1", "http://a"));
        assert!(bl.last_known_good("rtp1").is_none());
    }
}
