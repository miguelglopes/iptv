// Bootstrap measurement sweep.
//
// One-shot probe path: for each (stream_id, host) that lacks samples, fetch
// the m3u8, detect the auth-saturated placeholder shape (skip if so), pick
// one #EXTINF + segment URL, fetch the segment bytes, run `classify_ts_chunk`
// (with SPS-NAL extension) on them, compute bitrate from byte count and
// duration, and push a single `Sample` into the store.
//
// This shares the same TS-classification path as per-play — no ffprobe.
// Radio is skipped entirely (master playlist with raw AAC segments, see
// docs/plan-measured-quality.md §4 pre-flight).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use url::Url;

use crate::codec::classify_ts_chunk;
use crate::measured::{MeasuredStore, Sample, SampleSource};
use crate::state::AppState;
use crate::xtream::ChannelKind;

const BOOTSTRAP_GAP: Duration = Duration::from_millis(500);
const SEGMENT_MIN_BYTES: usize = 10_000;
const BITRATE_MIN_KBPS: u32 = 200;
const MIN_PIXELS: u64 = 320 * 240;
/// Default sweep concurrency when `max_connections` hasn't been discovered yet.
const DEFAULT_PROBE_BUDGET: u32 = 1;

/// Two cheap markers identify the auth-saturated placeholder a provider returns
/// when its slot is busy: an `#EXT-X-ENDLIST` (live streams don't normally
/// emit one) plus ≤2 `#EXTINF` entries pointing at a `black.ts` body.
pub fn is_placeholder_manifest(text: &str) -> bool {
    if !text.contains("#EXT-X-ENDLIST") {
        return false;
    }
    let extinf_count = text.lines().filter(|l| l.starts_with("#EXTINF")).count();
    extinf_count <= 2
}

/// Parse a media playlist for its first segment URL + duration. Resolves
/// relative segment URLs against `base_url`. Returns None for empty
/// playlists, parse failures, or master playlists (no #EXTINF lines).
fn first_extinf_and_url(body: &str, base_url: &str) -> Option<(f32, String)> {
    let base = Url::parse(base_url).ok()?;
    let mut pending_duration: Option<f32> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("#EXTINF:") {
            // "#EXTINF:5.0," — duration is before the comma.
            let dur_str = rest.split(',').next().unwrap_or(rest);
            if let Ok(d) = dur_str.parse::<f32>() {
                if d > 0.0 {
                    pending_duration = Some(d);
                }
            }
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(d) = pending_duration {
            let resolved = base.join(trimmed).ok()?;
            return Some((d, resolved.to_string()));
        }
    }
    None
}

/// Single-shot measurement against one upstream `manifest_url`. Returns
/// `None` for placeholder responses, network errors, classification
/// failures, or implausibly-small samples.
pub async fn measure_once(client: &Client, manifest_url: &str) -> Option<Sample> {
    // Step 1: fetch manifest.
    let body = match client.get(manifest_url).send().await {
        Ok(r) => match r.error_for_status() {
            Ok(r) => match r.text().await {
                Ok(t) => t,
                Err(e) => {
                    debug!(url = manifest_url, err = %e, "sweep: manifest body read failed");
                    return None;
                }
            },
            Err(e) => {
                debug!(url = manifest_url, err = %e, "sweep: manifest non-2xx");
                return None;
            }
        },
        Err(e) => {
            debug!(url = manifest_url, err = %e, "sweep: manifest fetch failed");
            return None;
        }
    };
    if is_placeholder_manifest(&body) {
        debug!(url = manifest_url, "sweep: placeholder manifest, skipping");
        return None;
    }

    // Step 2: find first segment URL + its duration.
    let (duration, segment_url) = first_extinf_and_url(&body, manifest_url)?;

    // Step 3: fetch one segment.
    let bytes = match client.get(&segment_url).send().await {
        Ok(r) => match r.error_for_status() {
            Ok(r) => match r.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    debug!(url = %segment_url, err = %e, "sweep: segment body read failed");
                    return None;
                }
            },
            Err(e) => {
                debug!(url = %segment_url, err = %e, "sweep: segment non-2xx");
                return None;
            }
        },
        Err(e) => {
            debug!(url = %segment_url, err = %e, "sweep: segment fetch failed");
            return None;
        }
    };
    if bytes.len() < SEGMENT_MIN_BYTES {
        debug!(url = %segment_url, len = bytes.len(), "sweep: segment too small");
        return None;
    }

    // Step 4: classify. TV always TS here (radio is skipped at sweep entry).
    let cls = classify_ts_chunk(&bytes)?;
    let kbps = (bytes.len() as f64 * 8.0 / 1000.0 / duration as f64) as u32;

    // Step 5: require a meaningful sample — codec + resolution must be set.
    // First-segment-of-stream may not contain an SPS NAL if the segment
    // happens to start mid-GOP. Without W/H we can't usefully rank, and
    // pushing a partial sample would burn a slot in the cap-5 buffer (and
    // potentially pollute the most-recent-stable-fields aggregator). Better
    // to drop it; the per-play path will pick the stream up once a user
    // actually watches it, and the next sweep cycle (if any) gets another
    // chance against a different segment.
    let codec = cls.codec_string()?;
    let width = cls.width?;
    let height = cls.height?;

    // Plausibility.
    if kbps < BITRATE_MIN_KBPS {
        debug!(url = %segment_url, kbps, "sweep: bitrate below floor");
        return None;
    }
    let pixels = width as u64 * height as u64;
    if pixels < MIN_PIXELS {
        debug!(url = %segment_url, width, height, "sweep: resolution below floor");
        return None;
    }

    Some(Sample {
        at: time::OffsetDateTime::now_utc(),
        source: SampleSource::Sweep,
        width,
        height,
        codec: Some(codec),
        pix_fmt: cls.pix_fmt.clone(),
        color_transfer: cls.color_transfer.clone(),
        framerate: cls.framerate,
        bitrate_kbps: Some(kbps),
    })
}

/// Run the one-time bootstrap sweep. Walks every (TV channel, source, alive
/// host) tuple in the catalog. Skips already-sampled keys. Clears
/// last_known_good on completion so prior pins don't override the new
/// measurement-driven ranking.
pub async fn run_bootstrap_sweep(state: Arc<AppState>) {
    // Wait briefly for catalog + host probes to populate. Otherwise the
    // first iteration runs against an empty catalog.
    loop {
        let snap = state.catalog.snapshot();
        let alive = state.hosts.alive_hosts_ranked();
        if !snap.channels.is_empty() && !alive.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Probe budget. `.max(1)` ensures the sweep runs even when max_cons=1
    // (it will collide with user plays — we accept that). `.min(2)` caps
    // sweep concurrency at the original 2.
    let max_cons = state.max_connections.load(Ordering::Acquire);
    let probe_budget = if max_cons == 0 {
        DEFAULT_PROBE_BUDGET
    } else {
        max_cons.saturating_sub(1).max(1).min(2)
    };
    let sema = Arc::new(Semaphore::new(probe_budget as usize));
    info!(probe_budget, max_cons, "bootstrap sweep starting");

    let snap = state.catalog.snapshot();
    let mut started = 0usize;
    let mut skipped = 0usize;
    for channel in &snap.channels {
        // Radio: master playlist with raw AAC segments — TS classifier can't
        // help and the master has no #EXTINF to sample. Plan §4 pre-flight 4.
        if channel.kind == ChannelKind::Radio {
            continue;
        }
        for source in &channel.sources {
            // For Xtream TV, fan out across all alive hosts so we get one
            // sample per (stream_id, host) pair.
            let alive = state.hosts.alive_hosts_ranked();
            for host in &alive {
                if state.measured.has_samples(source.stream_id, host) {
                    skipped += 1;
                    continue;
                }
                // Yield to user plays. Only blocks new probe starts; an
                // in-flight probe still holds its slot until upstream
                // completes (typically <2 s for a single segment fetch).
                while state.active_plays.load(Ordering::Relaxed) > 0 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                let permit = match sema.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let url = state.xtream.stream_url(host, source.stream_id, "m3u8");
                let client = state.upstream_http.clone();
                let store: Arc<MeasuredStore> = Arc::clone(&state.measured);
                let host_owned = host.clone();
                let sid = source.stream_id;

                tokio::spawn(async move {
                    if let Some(sample) = measure_once(&client, &url).await {
                        debug!(
                            stream_id = sid,
                            host = %host_owned,
                            w = sample.width,
                            h = sample.height,
                            codec = ?sample.codec,
                            kbps = ?sample.bitrate_kbps,
                            "sweep sample"
                        );
                        store.push(sid, &host_owned, sample);
                    }
                    drop(permit);
                });
                started += 1;
                tokio::time::sleep(BOOTSTRAP_GAP).await;
            }
        }
    }

    // Wait for in-flight probes to drain.
    let _drain = sema
        .acquire_many(probe_budget)
        .await
        .expect("semaphore not closed");

    info!(started, skipped, "bootstrap sweep complete; clearing last_known_good");
    state.blacklist.clear_last_known_good();
}

/// Spawn the bootstrap sweep on a background task.
pub fn spawn_bootstrap_sweep(state: Arc<AppState>) {
    tokio::spawn(async move {
        run_bootstrap_sweep(state).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_detected_when_endlist_and_two_extinfs() {
        let body = "#EXTM3U\n\
                    #EXT-X-VERSION:3\n\
                    #EXT-X-TARGETDURATION:5\n\
                    #EXTINF:5.000,\n\
                    http://cf/black.ts\n\
                    #EXT-X-ENDLIST\n";
        assert!(is_placeholder_manifest(body));
    }

    #[test]
    fn placeholder_detected_with_one_extinf() {
        let body = "#EXTM3U\n#EXTINF:1.5,\nhttp://cf/x.ts\n#EXT-X-ENDLIST\n";
        assert!(is_placeholder_manifest(body));
    }

    #[test]
    fn live_manifest_not_detected_as_placeholder() {
        let mut body = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n");
        for i in 0..10 {
            body.push_str(&format!("#EXTINF:5.000,\nhttp://cf/seg_{i}.ts\n"));
        }
        // No #EXT-X-ENDLIST — live playlist.
        assert!(!is_placeholder_manifest(&body));
    }

    #[test]
    fn vod_with_endlist_but_many_extinfs_not_a_placeholder() {
        // A real catch-up / VOD playlist has #EXT-X-ENDLIST but lots of segments.
        let mut body = String::from("#EXTM3U\n");
        for i in 0..50 {
            body.push_str(&format!("#EXTINF:5.000,\nhttp://cf/seg_{i}.ts\n"));
        }
        body.push_str("#EXT-X-ENDLIST\n");
        assert!(!is_placeholder_manifest(&body));
    }

    #[test]
    fn first_extinf_resolves_relative_url() {
        let body = "#EXTM3U\n#EXTINF:5.5,\nseg_001.ts\n#EXTINF:5.5,\nseg_002.ts\n";
        let (d, u) = first_extinf_and_url(body, "http://cf/live/playlist.m3u8").unwrap();
        assert!((d - 5.5).abs() < 0.001);
        assert_eq!(u, "http://cf/live/seg_001.ts");
    }

    #[test]
    fn first_extinf_resolves_absolute_url() {
        let body = "#EXTM3U\n#EXTINF:3.0,\nhttp://other.example/seg.ts\n";
        let (d, u) = first_extinf_and_url(body, "http://cf/live/playlist.m3u8").unwrap();
        assert!((d - 3.0).abs() < 0.001);
        assert_eq!(u, "http://other.example/seg.ts");
    }

    #[test]
    fn first_extinf_returns_none_for_master_playlist() {
        let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\nchunklist.m3u8\n";
        assert!(first_extinf_and_url(body, "http://cf/master.m3u8").is_none());
    }

    #[test]
    fn first_extinf_returns_none_for_empty_playlist() {
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n";
        assert!(first_extinf_and_url(body, "http://cf/x.m3u8").is_none());
    }
}
