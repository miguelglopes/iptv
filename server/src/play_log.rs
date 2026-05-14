// Bounded ring buffer of recent playback attempts. Each successful or failed
// /play/<key> request appends one PlayEvent that captures which upstream
// candidates were tried, how long each took, and the final outcome. Exposed
// via GET /admin/recent-plays for after-the-fact diagnosis ("why did RTP 1
// take so many retries at 12:10 UTC?").

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::Mutex;
use serde::Serialize;
use time::OffsetDateTime;

const CAPACITY: usize = 50;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptOutcome {
    Ok,
    Err { reason: String },
    Timeout,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayAttempt {
    pub host: String,
    pub url: String,
    pub elapsed_ms: u64,
    pub outcome: AttemptOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayEvent {
    pub id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started: OffsetDateTime,
    pub channel: String,
    pub catchup: bool,
    pub total_ms: u64,
    pub candidates_total: usize,
    pub succeeded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub attempts: Vec<PlayAttempt>,
}

pub struct PlayLog {
    counter: AtomicU32,
    events: Mutex<VecDeque<PlayEvent>>,
}

impl PlayLog {
    pub fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            events: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        }
    }

    // 6-hex id from a wrapping atomic counter — wraps every ~16M plays. The
    // server doesn't keep state across restarts so collisions don't matter;
    // the buffer holds 50 events max, anything older isn't there to clash with.
    pub fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:06x}", n & 0xff_ffff)
    }

    pub fn record(&self, event: PlayEvent) {
        let mut g = self.events.lock();
        if g.len() == CAPACITY {
            g.pop_front();
        }
        g.push_back(event);
    }

    // Newest first.
    pub fn snapshot(&self) -> Vec<PlayEvent> {
        let g = self.events.lock();
        g.iter().rev().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, channel: &str) -> PlayEvent {
        PlayEvent {
            id: id.into(),
            started: OffsetDateTime::now_utc(),
            channel: channel.into(),
            catchup: false,
            total_ms: 100,
            candidates_total: 1,
            succeeded: true,
            error: None,
            attempts: vec![],
        }
    }

    #[test]
    fn ids_are_unique_six_hex() {
        let log = PlayLog::new();
        let ids: Vec<String> = (0..3).map(|_| log.next_id()).collect();
        assert_eq!(ids[0], "000000");
        assert_eq!(ids[1], "000001");
        assert_eq!(ids[2], "000002");
        assert!(ids[0].len() == 6);
    }

    #[test]
    fn ring_evicts_oldest_at_capacity() {
        let log = PlayLog::new();
        for i in 0..(CAPACITY + 5) {
            log.record(event(&format!("{i:06x}"), "rtp1"));
        }
        let snap = log.snapshot();
        assert_eq!(snap.len(), CAPACITY);
        // newest first; the most-recently recorded was (CAPACITY + 4)
        assert_eq!(snap[0].id, format!("{:06x}", CAPACITY + 4));
        // oldest 5 were evicted; the 5th-from-end was id 5
        assert_eq!(snap.last().unwrap().id, format!("{:06x}", 5));
    }
}
