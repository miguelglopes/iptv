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
use tracing::{debug, info};
use url::Url;

use crate::codec::{classify_aac_chunk, classify_ts_chunk};
use crate::measured::{MeasuredStore, Sample, SampleSource};
use crate::state::AppState;
use crate::xtream::ChannelKind;

const BOOTSTRAP_GAP: Duration = Duration::from_millis(500);
const SEGMENT_MIN_BYTES: usize = 10_000;
const BITRATE_MIN_KBPS: u32 = 200;
const MIN_PIXELS: u64 = 320 * 240;

/// Strip a URL down to `scheme://host` for the measurement-cache `host`
/// key. Radio direct URLs need this — the cache is keyed by host root,
/// not the full URL. Falls back to empty on a malformed input (the
/// caller's `has_samples` check will then never match, which is fine —
/// the only consequence is an extra probe).
fn derive_host(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => match u.host_str() {
            Some(h) => format!("{}://{}", u.scheme(), h),
            None => String::new(),
        },
        Err(_) => String::new(),
    }
}
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

/// Single-shot TV measurement against one upstream `manifest_url`.
/// Returns `None` for placeholder responses, network errors, TS
/// classification failures, or implausibly-small samples.
///
/// Phase 8 renamed `measure_once` → `measure_once_tv` and added
/// `measure_once_audio` (parallel path for radio ADTS); call sites pick
/// the right one based on the channel's `ChannelKind`.
pub async fn measure_once_tv(client: &Client, manifest_url: &str) -> Option<Sample> {
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
        dvb_unsafe: Some(cls.dvb_unsafe),
        sample_rate_hz: None,
        audio_channels: None,
    })
}

/// Audio (radio) counterpart to `measure_once_tv`. Same shape:
///   1. Fetch master/chunklist manifest.
///   2. Pick the first EXTINF + segment URL.
///   3. Fetch the segment bytes (raw ADTS frames concatenated).
///   4. Run `classify_aac_chunk` to extract sample rate / channels / kbps.
///   5. Build a Sample with the audio-only fields populated and the
///      video fields (width/height/codec/...) left empty.
///
/// Audio segments are typically 30-200 KB at 64-320 kbps, well above the
/// TV path's 10 KB floor — but a low-bitrate stream might hit the lower
/// bound, so we drop the segment-size floor for audio and rely on
/// `classify_aac_chunk`'s built-in 16 kbps sanity filter.
pub async fn measure_once_audio(client: &Client, manifest_url: &str) -> Option<Sample> {
    let body = match client.get(manifest_url).send().await {
        Ok(r) => match r.error_for_status() {
            Ok(r) => match r.text().await {
                Ok(t) => t,
                Err(e) => {
                    debug!(url = manifest_url, err = %e, "audio sweep: manifest body read failed");
                    return None;
                }
            },
            Err(e) => {
                debug!(url = manifest_url, err = %e, "audio sweep: manifest non-2xx");
                return None;
            }
        },
        Err(e) => {
            debug!(url = manifest_url, err = %e, "audio sweep: manifest fetch failed");
            return None;
        }
    };
    if is_placeholder_manifest(&body) {
        debug!(url = manifest_url, "audio sweep: placeholder manifest");
        return None;
    }
    let (duration, segment_url) = first_extinf_and_url(&body, manifest_url)?;

    // For nested HLS (master → chunklist), the first EXTINF target may be
    // another playlist. We don't recursively walk here — at most one hop.
    // Heuristic: if it ends in `.m3u8`, fetch and re-resolve once.
    let segment_url = if segment_url.ends_with(".m3u8") {
        let nested = match client.get(&segment_url).send().await {
            Ok(r) => match r.error_for_status() {
                Ok(r) => match r.text().await {
                    Ok(t) => t,
                    Err(_) => return None,
                },
                Err(_) => return None,
            },
            Err(_) => return None,
        };
        let (_d2, inner_url) = first_extinf_and_url(&nested, &segment_url)?;
        inner_url
    } else {
        segment_url
    };

    let bytes = match client.get(&segment_url).send().await {
        Ok(r) => match r.error_for_status() {
            Ok(r) => match r.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    debug!(url = %segment_url, err = %e, "audio sweep: segment body read failed");
                    return None;
                }
            },
            Err(e) => {
                debug!(url = %segment_url, err = %e, "audio sweep: segment non-2xx");
                return None;
            }
        },
        Err(e) => {
            debug!(url = %segment_url, err = %e, "audio sweep: segment fetch failed");
            return None;
        }
    };
    let cls = classify_aac_chunk(&bytes, duration as f64)?;
    Some(Sample {
        at: time::OffsetDateTime::now_utc(),
        source: SampleSource::Sweep,
        width: 0,
        height: 0,
        codec: Some("aac".into()),
        pix_fmt: None,
        color_transfer: None,
        framerate: None,
        bitrate_kbps: cls.kbps,
        dvb_unsafe: None,
        sample_rate_hz: cls.sample_rate_hz,
        audio_channels: cls.audio_channels,
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
        max_cons.saturating_sub(1).clamp(1, 2)
    };
    let sema = Arc::new(Semaphore::new(probe_budget as usize));
    info!(probe_budget, max_cons, "bootstrap sweep starting");

    let snap = state.catalog.snapshot();
    let mut started = 0usize;
    let mut skipped = 0usize;
    for channel in &snap.channels {
        let is_radio = channel.kind == ChannelKind::Radio;
        for source in &channel.sources {
            // Per-source enumeration depends on kind:
            //   - Radio: each `direct_source` URL is its own (stream_id, host)
            //     measurement target; no host fanout. Restricted to HLS —
            //     `measure_once_audio` parses `#EXTINF` and would otherwise
            //     download an entire Mp3/Aac/Icecast body before bailing.
            //     Non-HLS radio still gets measured per-play via
            //     `proxy::proxy_segment` when a user actually tunes in.
            //   - TV: fan out across alive hosts so we get one sample per
            //     (stream_id, host) pair.
            let targets: Vec<(String, String)> = if is_radio {
                if source.radio_format != Some(crate::radio::RadioFormat::Hls) {
                    continue;
                }
                if let Some(direct) = source.direct_source.as_ref() {
                    let host = derive_host(direct);
                    vec![(host, direct.clone())]
                } else {
                    continue;
                }
            } else {
                let alive = state.hosts.alive_hosts_ranked();
                alive
                    .into_iter()
                    .map(|h| {
                        let url = state.xtream.stream_url(&h, source.stream_id, "m3u8");
                        (h, url)
                    })
                    .collect()
            };

            for (host, url) in targets {
                if state.measured.has_samples(source.stream_id, &host) {
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
                let client = state.upstream_http.clone();
                let store: Arc<MeasuredStore> = Arc::clone(&state.measured);
                let sid = source.stream_id;
                let radio = is_radio;

                tokio::spawn(async move {
                    let sample = if radio {
                        measure_once_audio(&client, &url).await
                    } else {
                        measure_once_tv(&client, &url).await
                    };
                    if let Some(sample) = sample {
                        debug!(
                            stream_id = sid,
                            host = %host,
                            kind = if radio { "radio" } else { "tv" },
                            w = sample.width,
                            h = sample.height,
                            codec = ?sample.codec,
                            kbps = ?sample.bitrate_kbps,
                            sample_rate = ?sample.sample_rate_hz,
                            channels = ?sample.audio_channels,
                            "sweep sample"
                        );
                        store.push(sid, &host, sample);
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

    // No more `clear_last_known_good()` here — Step 5 turned LKG into a
    // decayed rank-tuple bonus rather than a post-sort promotion, so old
    // pre-measurement pins no longer dominate the ranking once measured
    // siblings exist (they just contribute a small `lkg_bonus`).
    info!(started, skipped, "bootstrap sweep complete");
}

// --- Freshness loop (Step 6) ------------------------------------------------

/// Default freshness-loop interval when `freshness_loop_interval_secs = None`
/// AND `max_connections >= 3`. Plan §6: "auto-gated by `max_connections`".
/// 15 minutes is generous — long enough that the loop's bandwidth/CPU cost
/// is negligible, short enough that a measurement is at most ~30 min stale
/// before the next re-probe pass touches it.
const FRESHNESS_AUTO_INTERVAL_SECS: u64 = 900;

/// Decide whether to run the freshness loop and at what cadence. Pure
/// function so unit tests can pin the per-`max_connections` decision
/// table without spinning up the full loop. `None` means OFF.
///
/// Plan §6 table:
///   - `Some(0)` → force-off (incident-response escape hatch).
///   - `Some(n>0)` → force-on at `n` regardless of `max_connections`.
///   - `None` → auto: off when `max_cons ≤ 2`, on at 900 s when `≥ 3`.
fn freshness_interval(
    override_secs: Option<u64>,
    max_cons: u32,
) -> Option<u64> {
    match override_secs {
        Some(0) => None,
        Some(n) => Some(n),
        None => {
            if max_cons >= 3 {
                Some(FRESHNESS_AUTO_INTERVAL_SECS)
            } else {
                None
            }
        }
    }
}

/// Dynamic concurrency cap per plan §6. Leaves at least one upstream slot
/// for the user at all times. Recomputed each time a new probe is about
/// to be enqueued so the loop naturally backs off when plays spin up.
fn freshness_cap(max_cons: u32, active_plays: usize) -> u32 {
    max_cons
        .saturating_sub(active_plays as u32 + 1)
        .min(2)
}

/// True if `most_recent` is older than `ttl_secs` from `now` — or if no
/// sample has been recorded yet. Pure function so the freshness-skip
/// decision is testable without a MeasuredStore fixture.
fn needs_reprobe(
    most_recent: Option<time::OffsetDateTime>,
    now: time::OffsetDateTime,
    ttl_secs: u64,
) -> bool {
    match most_recent {
        None => true,
        Some(at) => (now - at).whole_seconds().max(0) as u64 >= ttl_secs,
    }
}

/// Reinforced pre-validation pass. Runs forever in the background after
/// the bootstrap sweep. Each tick walks every (TV channel × source × alive
/// host) tuple, skipping keys whose most-recent sample is within
/// `freshness_ttl_secs`. The rest get a fresh `measure_once` probe whose
/// `Sample::Sweep` lands in `MeasuredStore` so the rank tuple has
/// non-stale measurements for every play-time candidate.
///
/// Gating + concurrency follow plan §6: `freshness_interval` decides
/// whether to fire at all this iteration; `freshness_cap` recomputes per
/// probe to back off when users are watching.
pub async fn run_freshness_loop(state: Arc<AppState>) {
    loop {
        let override_secs = state.config.probe.freshness_loop_interval_secs;
        let max_cons = state.max_connections.load(Ordering::Acquire);

        // R2 issue 5: previously the loop unconditionally waited for
        // `max_connections > 0` before reading the override. That meant
        // `Some(n>0)` (force-on) was silently ignored at cold-start when
        // the provider hadn't reported its slot budget yet. Now: only
        // the auto-gated path (`None`) waits — force-on / force-off
        // read the override directly and don't care about `max_cons`.
        if override_secs.is_none() && max_cons == 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let Some(interval_secs) = freshness_interval(override_secs, max_cons) else {
            // Currently off — re-evaluate every minute in case max_connections
            // climbs above the threshold mid-run (provider reconfigured, etc.).
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        };
        debug!(
            interval_secs,
            max_cons,
            override_secs = ?override_secs,
            "freshness loop tick start"
        );
        // Pass a non-zero baseline cap so force-on at max_cons == 0 still
        // runs at least one probe per tick; `freshness_cap` clamps the
        // dynamic value at runtime as plays start/stop.
        let baseline_cap = if max_cons > 0 { max_cons } else { 2 };
        run_freshness_pass(&state, baseline_cap).await;
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// One pass: walk (channel × source × host), re-probing stale keys subject
/// to the dynamic concurrency cap and the active-play yield.
async fn run_freshness_pass(state: &Arc<AppState>, initial_max_cons: u32) {
    let ttl = state.config.probe.freshness_ttl_secs;
    let snap = state.catalog.snapshot();
    let in_flight: Arc<std::sync::atomic::AtomicUsize> =
        Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut probed = 0usize;
    let mut skipped_fresh = 0usize;

    for channel in &snap.channels {
        let is_radio = channel.kind == ChannelKind::Radio;
        for source in &channel.sources {
            // Same per-source target enumeration as the bootstrap sweep.
            // Radio sources are direct URLs (one target each), restricted to
            // HLS for the same reason as above; TV sources fan out across
            // alive hosts.
            let targets: Vec<(String, String)> = if is_radio {
                if source.radio_format != Some(crate::radio::RadioFormat::Hls) {
                    continue;
                }
                if let Some(direct) = source.direct_source.as_ref() {
                    let host = derive_host(direct);
                    vec![(host, direct.clone())]
                } else {
                    continue;
                }
            } else {
                state
                    .hosts
                    .alive_hosts_ranked()
                    .into_iter()
                    .map(|h| {
                        let url = state.xtream.stream_url(&h, source.stream_id, "m3u8");
                        (h, url)
                    })
                    .collect()
            };

            for (host, url) in targets {
                let now = time::OffsetDateTime::now_utc();
                let most_recent = state.measured.most_recent_at(source.stream_id, &host);
                if !needs_reprobe(most_recent, now, ttl) {
                    skipped_fresh += 1;
                    continue;
                }
                // Wait for capacity: back off while users are watching, and
                // while we're already at the cap. The cap can change between
                // iterations as max_connections is re-discovered or plays
                // start/stop.
                loop {
                    let active = state.active_plays.load(Ordering::Relaxed);
                    let mc = state.max_connections.load(Ordering::Acquire).max(initial_max_cons);
                    let cap = freshness_cap(mc, active) as usize;
                    let live = in_flight.load(Ordering::Acquire);
                    if cap > 0 && live < cap {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }

                in_flight.fetch_add(1, Ordering::AcqRel);
                let client = state.upstream_http.clone();
                let store: Arc<MeasuredStore> = Arc::clone(&state.measured);
                let in_flight2 = Arc::clone(&in_flight);
                let sid = source.stream_id;
                let radio = is_radio;
                tokio::spawn(async move {
                    let sample = if radio {
                        measure_once_audio(&client, &url).await
                    } else {
                        measure_once_tv(&client, &url).await
                    };
                    if let Some(sample) = sample {
                        debug!(
                            stream_id = sid,
                            host = %host,
                            kind = if radio { "radio" } else { "tv" },
                            kbps = ?sample.bitrate_kbps,
                            "freshness sample"
                        );
                        store.push(sid, &host, sample);
                    }
                    in_flight2.fetch_sub(1, Ordering::AcqRel);
                });
                probed += 1;
                // Same small inter-spawn gap as the bootstrap sweep —
                // gives the runtime a chance to schedule the just-spawned
                // task before queueing the next one.
                tokio::time::sleep(BOOTSTRAP_GAP).await;
            }
        }
    }

    // Drain — wait for the last spawned probes to complete so the next
    // tick starts from a clean slate.
    while in_flight.load(Ordering::Acquire) > 0 {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    info!(probed, skipped_fresh, "freshness pass complete");
}

/// Spawn the freshness loop on a background task. Called from main.rs
/// alongside `spawn_bootstrap_sweep`; both run for the process lifetime.
pub fn spawn_freshness_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        run_freshness_loop(state).await;
    });
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

    // --- Freshness loop helpers (Step 6) ---------------------------------

    #[test]
    fn freshness_interval_force_off() {
        // Some(0) is the incident-response escape hatch — disable regardless
        // of provider capacity.
        assert_eq!(freshness_interval(Some(0), 8), None);
    }

    #[test]
    fn freshness_interval_force_on() {
        // Some(n>0) wins over the auto-gating decision. Useful for testing
        // and for measured providers where we want a tighter freshness
        // budget than the auto default. R2 issue 5: must honour even at
        // max_cons == 0 (cold start before the probe loop reports).
        assert_eq!(freshness_interval(Some(60), 1), Some(60));
        assert_eq!(freshness_interval(Some(60), 0), Some(60));
        assert_eq!(freshness_interval(Some(60), 8), Some(60));
    }

    #[test]
    fn freshness_interval_auto_off_when_low_max_cons() {
        // None + low max_cons → off. Plan §6: max_cons ≤ 2 has no idle
        // headroom for a background probe loop.
        assert_eq!(freshness_interval(None, 0), None);
        assert_eq!(freshness_interval(None, 1), None);
        assert_eq!(freshness_interval(None, 2), None);
    }

    #[test]
    fn freshness_interval_auto_on_at_3_plus() {
        // None + max_cons ≥ 3 → 900 s. Plan §6.
        assert_eq!(freshness_interval(None, 3), Some(FRESHNESS_AUTO_INTERVAL_SECS));
        assert_eq!(freshness_interval(None, 8), Some(FRESHNESS_AUTO_INTERVAL_SECS));
    }

    #[test]
    fn freshness_cap_leaves_one_slot_for_user() {
        // max_cons - (active_plays + 1), capped at 2.
        assert_eq!(freshness_cap(3, 0), 2); // 3-1=2, cap=2
        assert_eq!(freshness_cap(3, 1), 1); // 3-2=1
        assert_eq!(freshness_cap(3, 2), 0); // 3-3=0
        assert_eq!(freshness_cap(8, 0), 2); // capped at 2 even with lots of slack
        assert_eq!(freshness_cap(1, 0), 0); // single-slot provider: no headroom
    }

    #[test]
    fn needs_reprobe_when_no_sample() {
        let now = time::OffsetDateTime::now_utc();
        assert!(needs_reprobe(None, now, 3600));
    }

    #[test]
    fn needs_reprobe_when_sample_older_than_ttl() {
        let now = time::OffsetDateTime::now_utc();
        let stale = now - Duration::from_secs(4000);
        assert!(needs_reprobe(Some(stale), now, 3600));
    }

    #[test]
    fn skips_reprobe_when_sample_within_ttl() {
        let now = time::OffsetDateTime::now_utc();
        let fresh = now - Duration::from_secs(300); // 5 min ago, ttl 1h
        assert!(!needs_reprobe(Some(fresh), now, 3600));
    }
}
