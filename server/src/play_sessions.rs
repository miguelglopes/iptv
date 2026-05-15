// Per-play upstream attribution.
//
// The /api/feedback endpoint needs to know *exactly* which upstream a failing
// client was playing so it can blame the right one. Reading the global
// last-known-good is racy: another client may have played the same channel
// in between, and LKG would point at the new upstream.
//
// Fix: every play attempt gets a `pid` (client-generated short hex, baked into
// the play URL as `?pid=…`). When the proxy chooses an upstream for that
// request, it records (pid → upstream) here. When the client later reports
// a failure with the same pid, the feedback handler looks up the actual
// upstream that was served — no LKG races.
//
// Entries are TTL'd because most plays never see a feedback callback (the
// stream just keeps working). 10 min is generous: typical failure feedback
// fires within seconds of play, but a long-watching session that finally
// drops should still attribute correctly.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

const ENTRY_TTL: Duration = Duration::from_secs(600);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct Entry {
    upstream: String,
    channel_key: String,
    inserted: Instant,
}

#[derive(Default)]
struct Inner {
    by_pid: HashMap<String, Entry>,
    last_sweep: Option<Instant>,
}

#[derive(Default)]
pub struct PlaySessions {
    inner: RwLock<Inner>,
}

impl PlaySessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the upstream URL the proxy actually served for this pid.
    /// Overwrites any prior entry (a re-play with the same pid uses the
    /// fresh upstream).
    pub fn note(&self, pid: &str, channel_key: &str, upstream: &str) {
        if pid.is_empty() {
            return;
        }
        let mut g = self.inner.write();
        g.by_pid.insert(
            pid.to_string(),
            Entry {
                upstream: upstream.to_string(),
                channel_key: channel_key.to_string(),
                inserted: Instant::now(),
            },
        );
        maybe_sweep(&mut g);
    }

    /// Look up the upstream associated with this pid. Returns the URL plus
    /// the channel key (cross-check with the feedback path's channel param —
    /// mismatches mean the pid is stale or forged; the caller should ignore).
    pub fn lookup(&self, pid: &str) -> Option<(String, String)> {
        if pid.is_empty() {
            return None;
        }
        let g = self.inner.read();
        let entry = g.by_pid.get(pid)?;
        if entry.inserted.elapsed() > ENTRY_TTL {
            return None;
        }
        Some((entry.upstream.clone(), entry.channel_key.clone()))
    }
}

fn maybe_sweep(inner: &mut Inner) {
    let now = Instant::now();
    if let Some(last) = inner.last_sweep {
        if now.duration_since(last) < SWEEP_INTERVAL {
            return;
        }
    }
    inner.last_sweep = Some(now);
    inner
        .by_pid
        .retain(|_, e| now.duration_since(e.inserted) <= ENTRY_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_then_lookup_returns_upstream_and_channel() {
        let s = PlaySessions::new();
        s.note("ab12cd", "rtp1", "http://cf.example/live/U/P/123.m3u8");
        let (url, ch) = s.lookup("ab12cd").expect("present");
        assert_eq!(url, "http://cf.example/live/U/P/123.m3u8");
        assert_eq!(ch, "rtp1");
    }

    #[test]
    fn lookup_returns_none_for_unknown_pid() {
        let s = PlaySessions::new();
        assert!(s.lookup("missing").is_none());
    }

    #[test]
    fn empty_pid_is_inert() {
        let s = PlaySessions::new();
        s.note("", "rtp1", "http://x");
        assert!(s.lookup("").is_none());
    }

    #[test]
    fn note_overwrites_prior_upstream_for_same_pid() {
        let s = PlaySessions::new();
        s.note("p1", "rtp1", "http://a");
        s.note("p1", "rtp1", "http://b");
        let (url, _) = s.lookup("p1").unwrap();
        assert_eq!(url, "http://b");
    }
}
