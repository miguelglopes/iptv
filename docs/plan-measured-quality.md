# Measured stream quality → variant ranking

## Goals

Stop trusting upstream names. Stop trusting one-shot measurements. Rank variants on what we actually observe — per host, over a rolling window, weighted by whether plays actually succeed. Defend the cache from provider "auth-saturated" placeholders that would otherwise poison it. Extract every signal from TS bytes the proxy already buffers, so per-play refreshes data organically and sweep is one-shot bootstrap-only — and **shares the same TS-classification code path** rather than introducing a second extractor.

## Philosophy

1. **Measurements beat labels.** The regex is gone from ranking. It survives only as the implicit catalog-build order for unmeasured siblings.
2. **History beats theory.** A 1080p source that worked 2/10 times sits behind a 720p sibling that worked 9/10. Quality is the tiebreaker among things that actually work.
3. **Robust beats precise.** Sample buffer of 5 most-recent observations per `(stream_id, host)`; median for bitrate (one placeholder sample can't poison the cache); most-recent for stable fields. One bad probe is rounded away.
4. **Defensive against gaslighting.** The provider returns a tiny `black.ts` placeholder playlist when its connection slot is busy. We detect that response and refuse to record it (sweep) *or* serve it (per-play). It's treated as a failure, not as data.
5. **One mechanism for measurement.** Both sweep and per-play hand a fully-formed `Sample` to `MeasuredStore::push` via the same `classify_ts_chunk` extension. Sweep is the one-shot variant; per-play is the streaming variant. No `ffprobe`. No second extractor.
6. **Less work, organically refreshed.** Per-play covers every channel users actually watch. Sweep is bootstrap-only for unwatched channels. No periodic re-sweep — watched channels refresh themselves every play.

## What's already in the tree (don't reinvent)

These landed on the `radio-mode` branch and the plan rides on them:

- **`probe=true` query param + `track_failures` plumbing** through `play_playlist` → `fetch_and_rewrite_playlist` → `rewrite_playlist` → `proxy_url` → `SegmentToken` → `mark_segment_failure`. Today it's used by the client capability probe (`/api/probe/{audio,video}.m3u8` redirects to `/play/<top-channel>?probe=1`, exercised by `app/js/caps.js`). The plan's placeholder-bail logic uses the same gate: just `anyhow::bail!` inside `fetch_and_rewrite_playlist` and the existing `!probe_request` check in the caller decides whether to call `note_url_failed`.
- **`ChannelKind::Tv` / `Radio`** in `CanonicalChannel` (`canonical.rs:196`). Radio sources carry `direct_source: Some(url)` and build_candidates emits one candidate per source instead of fanning out across hosts (`proxy.rs:249`). The plan's per-host keying naturally handles both: for radio, the host is derived from `direct_source`'s authority.
- **`handle_ts_segment`** (`proxy.rs:545`) already classifies every TS segment with a known stream_id and caches the result. The plan's per-play extraction extends `Classification` and pushes into a sample accumulator from that exact callsite.
- **HEVC blanket-filter is gone** (commit `546af6c`). HEVC sources are eligible candidates; HDR ranking is live (subject to pre-flight check 3 below).

## Non-goals

- Catalog dedup / canonical-key derivation.
- Host-latency ordering inside `build_candidates` (kept as the implicit tie-break — stable sort preserves it among rank-equal candidates).
- Client-side measurement.
- Regex `score_variant` — kept only as catalog-build ordering for unmeasured siblings, never overrides a measurement.
- Periodic re-sweep of stale entries — per-play is the refresh mechanism. If a channel goes long enough unwatched that its samples are stale, nobody can tell.
- `ffprobe` and any `ffmpeg` dependency — sweep uses the same TS classifier as per-play.
- Client capability probe (`/api/probe/*`, `X-Client-Caps`, `caps_required` filter) — orthogonal infrastructure that already exists.

## Pre-flight (do these before writing code)

1. **Manifest type.** Confirm the provider serves media playlists, not master playlists. If master, sampling has to walk variants:

   ```bash
   curl -s 'http://cf.<host>/live/USER/PASS/<stream_id>.m3u8' | head -20
   ```

   `#EXTINF` = media playlist (plan proceeds). `#EXT-X-STREAM-INF` = master (sampling strategy needs revision).

2. **Placeholder fingerprint.** Reproduce the auth-saturated response. Start a play on the TV (uses the only slot), then in parallel:

   ```bash
   curl -s 'http://cf.<host>/live/USER/PASS/<known_stream_id>.m3u8'
   ```

   Expect:

   ```
   #EXTM3U
   #EXT-X-VERSION:3
   #EXT-X-TARGETDURATION:5
   #EXTINF:5.000,
   http://cf.<host>/.../black.ts
   #EXT-X-ENDLIST
   ```

   Two cheap markers identify it: `#EXT-X-ENDLIST` present (live streams normally don't have one) + `#EXTINF` count ≤ 2. Capture the byte count + duration of `black.ts` for the plausibility thresholds.

3. **HEVC main10 on the B4 chipset.** Once the cache is populated, find a 10-bit HEVC source in `/admin/measured-quality`. Attempt playback on the TV; observe `canplay` + frames advancing. If fails: flip `TV_DECODES_HEVC_MAIN10` to `false` in `proxy.rs` and redeploy.

4. **Radio segment format.** ✅ Resolved against `streaming-live.rtp.pt` (the only radio host in `radios.m3u`). The provider serves:
   - **Master playlists** with one variant (`chunklist.m3u8`), so `measure_once` would fail at "find first #EXTINF" anyway.
   - **Raw AAC segments** (`audio/x-aac`, `.aac` extension), so `classify_ts_chunk` returns `None`.

   Combined with the URL-pattern mismatch in §3, **radio is entirely out of scope for measurement.** The sweep loop skips `ChannelKind::Radio`. Per-play already doesn't fire for radio. Radio canonical channels typically have one source each anyway, so variant ranking is moot.

## Approach

### 1. Per-host measured-quality cache with rolling sample buffer

New module `server/src/measured.rs`. Keyed by `(stream_id: u64, host: String)` — different hosts proxy to different backends with different delivery characteristics; the ranker must see that.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SampleSource { Sweep, PerPlay }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub at: SystemTime,
    pub source: SampleSource,
    pub width: u32,             // 0 for audio-only (radio)
    pub height: u32,            // 0 for audio-only (radio)
    pub codec: Option<String>,  // None for audio-only (radio)
    pub pix_fmt: Option<String>,
    pub color_transfer: Option<String>,
    pub framerate: Option<f32>,
    pub bitrate_kbps: Option<u32>,
}

const WINDOW: usize = 5;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MeasuredEntry {
    pub samples: VecDeque<Sample>,
}

impl MeasuredEntry {
    pub fn push(&mut self, s: Sample) {
        if self.samples.len() >= WINDOW { self.samples.pop_front(); }
        self.samples.push_back(s);
    }

    /// width/height/codec/pix_fmt/color_transfer/fps: most-recent (stable fields).
    /// bitrate_kbps: median of non-None samples (robust to one bad probe).
    pub fn aggregate(&self) -> Option<MeasuredQuality> {
        let last = self.samples.back()?;
        let mut bitrates: Vec<u32> = self.samples.iter()
            .filter_map(|s| s.bitrate_kbps)
            .collect();
        bitrates.sort_unstable();
        let bitrate_kbps = bitrates.get(bitrates.len() / 2).copied();
        Some(MeasuredQuality {
            width: last.width, height: last.height,
            codec: last.codec.clone(),
            pix_fmt: last.pix_fmt.clone(),
            color_transfer: last.color_transfer.clone(),
            framerate: last.framerate,
            bitrate_kbps,
            samples_count: self.samples.len(),
            measured_at: last.at,
        })
    }
}

pub type Key = (u64, String);

pub struct MeasuredStore {
    inner: RwLock<HashMap<Key, MeasuredEntry>>,
    path: PathBuf,
    dirty: AtomicBool,
}

impl MeasuredStore {
    pub fn load_or_empty(path: PathBuf) -> Self;
    pub fn get(&self, stream_id: u64, host: &str) -> Option<MeasuredQuality>;
    pub fn push(&self, stream_id: u64, host: &str, sample: Sample);
    pub fn has_samples(&self, stream_id: u64, host: &str) -> bool;
    pub fn snapshot(&self) -> Vec<(Key, Vec<Sample>)>;
}
```

Sweep and per-play both reach `MeasuredStore::push` with a complete `Sample`. One path into the buffer. Persisted JSON at `server/data/measured_quality.json`, atomic flush on 5 s debounce.

### 2. Full extraction inside `classify_ts_chunk`

`codec.rs::classify_ts_chunk` (already runs on every TS segment with known stream_id, `proxy.rs:545`). Extend the existing `Classification` struct so one call yields every quality signal we want.

```rust
pub struct Classification {
    pub video_codec: Option<VideoCodec>,    // existing
    pub video_pid: Option<u16>,             // existing
    pub pmt_pid: Option<u16>,               // existing
    pub pcr_pid: Option<u16>,               // existing
    pub subtitle_pids: Vec<u16>,            // existing
    pub width: Option<u32>,                 // NEW
    pub height: Option<u32>,                // NEW
    pub framerate: Option<f32>,             // NEW
    pub pix_fmt: Option<String>,            // NEW
    pub color_transfer: Option<String>,     // NEW
}
```

Parsing:

- **codec string** — derived from `video_codec`: `H264 → "h264"`, `Hevc → "hevc"`, `Other → None` (currently `Other` covers MPEG-2; if we ever care about ranking MPEG-2 lower, surface it as its own enum variant).
- **width / height / framerate / pix_fmt / color_transfer for H.264** — parse the first SPS NAL inside the video PID's first PES packet:
  - `width = (pic_width_in_mbs_minus1 + 1) * 16 - crop_left - crop_right`
  - `height = (pic_height_in_map_units_minus1 + 1) * 16 * (frame_mbs_only_flag ? 1 : 2) - crop_top - crop_bot`
  - framerate from VUI `time_scale / (2 * num_units_in_tick)` when `timing_info_present_flag`
  - `chroma_format_idc` + `bit_depth_luma_minus8` → `"yuv420p"` / `"yuv420p10le"` / etc.
  - VUI `colour_description_present_flag` → `transfer_characteristics` byte: 1 → `"bt709"`, 16 → `"smpte2084"`, 18 → `"arib-std-b67"`.
- **Same fields for HEVC** — different syntax (HEVC SPS has `profile_tier_level` + VUI), same outputs. Required because the HEVC filter is gone; HEVC sources need full ranking too.

For radio: see §3 — radio segments don't go through `handle_ts_segment` at all (URL pattern mismatch + likely non-TS container). Radio per-play measurement is intentionally out of scope; the sweep is the only path that ever touches radio.

### 3. Per-play session accumulator (streaming case)

One play session → one complete sample. No partial records, no second writer patching an existing sample. Both classifier and segment hot-path feed an in-progress map; a background committer drains entries that have gone quiet.

```rust
pub struct InProgress {
    width: Option<u32>,
    height: Option<u32>,
    codec: Option<String>,
    pix_fmt: Option<String>,
    color_transfer: Option<String>,
    framerate: Option<f32>,
    bitrate_ewma_kbps: Option<f32>,
    last_activity: Instant,
}

pub struct PerPlayAccumulator {
    inner: RwLock<HashMap<(u64, String), InProgress>>,
}

impl PerPlayAccumulator {
    /// From handle_ts_segment after classify_ts_chunk succeeds.
    /// Idempotent; refreshes last_activity on every call.
    pub fn note_classification(&self, sid: u64, host: &str, c: &Classification);

    /// From proxy_segment per segment with known stream_id + duration.
    pub fn note_segment_kbps(&self, sid: u64, host: &str, kbps: f32) {
        let mut g = self.inner.write();
        let ip = g.entry((sid, host.to_string())).or_default();
        const ALPHA: f32 = 0.3;
        ip.bitrate_ewma_kbps = Some(match ip.bitrate_ewma_kbps {
            Some(prev) => ALPHA * kbps + (1.0 - ALPHA) * prev,
            None => kbps,
        });
        ip.last_activity = Instant::now();
    }

    /// Background: every 5 s, commit entries idle for ≥30 s.
    pub async fn run_committer(self: Arc<Self>, store: Arc<MeasuredStore>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let now = Instant::now();
            let mut to_commit = Vec::new();
            {
                let mut g = self.inner.write();
                g.retain(|key, ip| {
                    if now.duration_since(ip.last_activity) >= Duration::from_secs(30) {
                        to_commit.push((key.clone(), ip.clone()));
                        false
                    } else { true }
                });
            }
            for ((sid, host), ip) in to_commit {
                if let Some(sample) = ip.into_sample(SampleSource::PerPlay) {
                    store.push(sid, &host, sample);
                }
            }
        }
    }
}
```

`InProgress::into_sample` returns `Some(Sample)` only when at least one classification arrived (otherwise we know nothing about the source).

**Per-play scope is TV-only.** `handle_ts_segment` (`proxy.rs:643`) is gated on `is_ts && stream_id.is_some()`. Radio segments fail both: their content-type isn't `mp2t`, their URLs aren't `.ts`, and `stream_id_from_source_url` (`proxy.rs:738`) only matches the Xtream `/live/USER/PASS/<sid>` pattern. Radio direct_source URLs slip past unrecognised. We are not extending the per-play path to handle radio — radio canonical channels typically have exactly one source, so variant ranking is moot for them.

#### Plumbing the segment hot-path

`fetch_and_rewrite_playlist` (`proxy.rs:318`) already sees `#EXTINF:` durations and the `track_failures` flag is already plumbed through. Stash segment duration and host on the `SegmentToken`:

```rust
struct SegmentToken {
    u: String,
    p: Option<String>,
    c: Option<String>,
    probe: bool,        // existing
    d: Option<f32>,     // NEW: segment duration from #EXTINF
    h: Option<String>,  // NEW: host (so we can key per-(stream_id, host))
}
```

In `proxy_segment` (`proxy.rs:524`-ish post-diff), after fetching upstream bytes, if `stream_id`, `d`, and `h` are all known and **`segment.probe == false`** (real plays only — probe-mode segments are the client capability probe, we don't want them shaping bitrate):

```rust
let kbps = (bytes.len() as f64 * 8.0 / 1000.0 / d as f64) as f32;
state.per_play.note_segment_kbps(stream_id, &h, kbps);
```

In `handle_ts_segment` after a successful classification (same `!segment.probe` gate):

```rust
state.per_play.note_classification(stream_id, &host, &classification);
```

Classifier cache (`codec.rs:55`) ensures SPS parse runs once per stream; subsequent calls are cache hits.

### 4. Bootstrap sweep — same classifier, one-shot

The sweep is a server-internal background loop. For each `(stream_id, host)` lacking samples, it:

1. Fetches the upstream m3u8 directly (no HTTP round-trip through `/play/`).
2. Checks for placeholder via `is_placeholder_manifest` — bail if true.
3. Picks one `#EXTINF` + segment URL from the manifest.
4. Fetches that segment's bytes from upstream.
5. Runs `classify_ts_chunk` (with §2 extensions) on the bytes.
6. Computes `bitrate_kbps = bytes.len() * 8 / 1000 / extinf_duration`.
7. Plausibility post-check: drop if `width * height` is non-zero AND `< 320*240`, OR `bitrate < 200 kbps`, OR fewer than ~10 KB of TS data returned.
8. Pushes a single complete `Sample` (with `SampleSource::Sweep`) to `MeasuredStore`.

```rust
let max_cons = state.xtream_user_info.max_connections.unwrap_or(2);
// `.max(1)` ensures the sweep runs even when max_cons=1 (it will collide
// with user plays — we accept that). `.min(2)` caps the original ceiling.
let probe_budget = (max_cons.saturating_sub(1)).max(1).min(2);
let sema = Arc::new(Semaphore::new(probe_budget));
let gap = Duration::from_millis(500);

for channel in catalog.channels() {
    // Radio is out of scope for measurement (pre-flight 4): master playlists
    // with raw AAC segments — classify_ts_chunk can't help, no #EXTINF in the
    // master to sample from, and radio channels are typically single-source so
    // ranking is moot.
    if channel.kind == ChannelKind::Radio { continue; }

    for source in &channel.sources {
        let probe_targets: Vec<(u64, String, String)> = hosts.alive_hosts_ranked()
            .into_iter()
            .map(|h| {
                let url = state.xtream.stream_url(&h, source.stream_id, "m3u8");
                (source.stream_id, h, url)
            })
            .collect();

        for (sid, host, url) in probe_targets {
            if state.measured.has_samples(sid, &host) { continue; }

            // Yield to user plays. Only blocks new probe starts; an in-flight
            // probe still holds its slot for up to upstream_timeout.
            while state.active_plays.load(Ordering::Relaxed) > 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }

            let permit = sema.acquire().await;
            let client = state.upstream_http.clone();
            let store = Arc::clone(&state.measured);

            tokio::spawn(async move {
                if let Some(sample) = probe::measure_once(&client, &url).await {
                    store.push(sid, &host, sample);
                }
                drop(permit);
            });
            tokio::time::sleep(gap).await;
        }
    }
}

// Initial sweep complete — flush LKG so prior pins don't override the new ranking.
state.blacklist.clear_last_known_good();
```

`probe::measure_once` implementation:

```rust
pub async fn measure_once(client: &Client, manifest_url: &str) -> Option<Sample> {
    // Step 1: fetch manifest
    let body = client.get(manifest_url).send().await.ok()?
        .error_for_status().ok()?.text().await.ok()?;
    if is_placeholder_manifest(&body) { return None; }

    // Step 2: find first segment URL + its #EXTINF duration
    let (duration, segment_url) = first_extinf_and_url(&body, manifest_url)?;

    // Step 3: fetch one segment
    let bytes = client.get(&segment_url).send().await.ok()?
        .error_for_status().ok()?.bytes().await.ok()?;
    if bytes.len() < 10_000 { return None; }

    // Step 4: classify (TV is always TS for this provider; if it ever fails
    // here it's a real upstream anomaly — drop the result rather than
    // recording an unclassifiable Sample).
    let cls = classify_ts_chunk(&bytes)?;
    let kbps = (bytes.len() as f64 * 8.0 / 1000.0 / duration as f64) as u32;

    // Step 5: plausibility
    if kbps < 200 { return None; }
    if let (Some(w), Some(h)) = (cls.width, cls.height) {
        if w as u64 * h as u64 != 0 && (w as u64 * h as u64) < 320 * 240 { return None; }
    }

    Some(Sample {
        at: SystemTime::now(),
        source: SampleSource::Sweep,
        width: cls.width.unwrap_or(0),
        height: cls.height.unwrap_or(0),
        codec: cls.codec_string(),
        pix_fmt: cls.pix_fmt.clone(),
        color_transfer: cls.color_transfer.clone(),
        framerate: cls.framerate,
        bitrate_kbps: Some(kbps),
    })
}

fn is_placeholder_manifest(text: &str) -> bool {
    if !text.contains("#EXT-X-ENDLIST") { return false; }
    let extinf_count = text.lines().filter(|l| l.starts_with("#EXTINF")).count();
    extinf_count <= 2
}
```

Budget table:

| `max_cons` | sweep concurrency | leaves free for user |
|---|---|---|
| 1 | 1 (collides with plays — accepted) | 0 |
| 2 | 1 | 1 |
| 3 | 2 | 1 |
| 5+ | 2 (cap) | 3+ |

After ~30 min of normal use, virtually every watched channel has at least one per-play sample, and the sweep stops touching them entirely. Unwatched channels keep their bootstrap data forever (or until a user watches them, at which point per-play takes over).

### 5. Placeholder defence in the playback hot path

In `fetch_and_rewrite_playlist` (`proxy.rs:318`), after fetching the body but **before** rewriting URIs:

```rust
if is_placeholder_manifest(body) {
    warn!(channel = %channel.key, url = %cand.url, "upstream returned placeholder manifest");
    anyhow::bail!("upstream returned placeholder (auth-saturated)");
}
```

Just bail. The existing `!probe_request` gate in `play_playlist` already calls `note_url_failed` on non-probe errors and skips it on probe-mode errors — placeholder detection inherits the correct behaviour for free.

Effect on the play_log:

- **Before:** the proxy serves black.ts to the TV as a successful play; `play_log` records success.
- **After:** the attempt is recorded `AttemptOutcome::Err { reason: "upstream returned placeholder (auth-saturated)" }`, the URL goes into the blacklist (only on real plays, not probes), the next candidate is tried, and the success_score (§7) sees the truth.

### 6. last_known_good invalidation

After the initial sweep completes:

```rust
state.blacklist.clear_last_known_good();
```

Without this, channels users have played before stay pinned to whatever they were tried with first, never picking up new measurement-driven ordering. New `Blacklist::clear_last_known_good` method (one-line `write().last_known_good.clear()`).

No second trigger — per-play measurements update samples but don't reveal a better sibling.

### 7. Scoring change — success_score ahead of quality

```rust
fn success_score(stream_id: u64, host: &str, log: &PlayLog) -> f32 {
    let snap = log.snapshot();  // newest first
    let mut sum_w = 0.0_f32;
    let mut sum = 0.0_f32;
    let mut decay = 1.0_f32;
    for ev in &snap {
        for att in &ev.attempts {
            if att.host != host { continue; }
            if stream_id_of_url(&att.url) != Some(stream_id) { continue; }
            let v = match att.outcome {
                AttemptOutcome::Ok => 1.0,
                AttemptOutcome::Err { .. } | AttemptOutcome::Timeout => 0.0,
            };
            sum += v * decay;
            sum_w += decay;
            decay *= 0.9;
        }
    }
    if sum_w == 0.0 { 0.5 } else { sum / sum_w }
}

fn success_bucket(score: f32) -> i32 { (score * 10.0).round() as i32 }   // 0..=10

const TV_DECODES_HEVC_MAIN10: bool = true;  // flip + redeploy if pre-flight 3 fails

fn source_rank_key(
    stream_id: u64, host: &str,
    measured: &MeasuredStore, log: &PlayLog,
) -> (i32, i32, i32, i32, i64, i32, i32) {
    let success = success_bucket(success_score(stream_id, host, log));
    match measured.get(stream_id, host) {
        Some(q) => {
            let pix_fmt_10bit = q.pix_fmt.as_deref().map(|p| p.contains("10")).unwrap_or(false);
            let is_hevc = q.codec.as_deref() == Some("hevc");
            let hdr_raw = hdr_rank(q.pix_fmt.as_deref(), q.color_transfer.as_deref());
            let hdr = if !TV_DECODES_HEVC_MAIN10 && is_hevc && pix_fmt_10bit { 0 } else { hdr_raw };
            (
                1,                                                    // measured > unmeasured
                success,                                              // history-aware
                hdr,                                                  // 10-bit / HDR (gated by main10)
                bpp_bucket(q.bitrate_kbps, q.width, q.height),        // bitrate-per-pixel (radio: -1, harmless tie)
                (q.width as u64 * q.height as u64) as i64,            // resolution (0 for radio)
                codec_rank(q.codec.as_deref()),                       // av1 > hevc > h264 > mpeg2
                fps_rank(q.framerate),                                // 50/60 > 25/30
            )
        }
        None => (0, success, 0, 0, 0, 0, 0),  // unmeasured but history still counts
    }
}
```

Radio canonical channels typically have one source each (one `direct_source` URL → one candidate), so the sort within a radio channel is a no-op. We don't engineer a `raw_bitrate_bucket` for the radio-multi-source case because that case essentially doesn't occur; if it ever does, all radio candidates rank at `bpp_bucket = -1` and the stable-sort order (catalog) decides between them.

Helper funcs `hdr_rank`, `bpp_bucket`, `codec_rank`, `fps_rank` unchanged.

Apply in `build_candidates` after computing the `(source × host)` matrix:

```rust
fresh.sort_by(|a, b| {
    let ka = source_rank_key(a.stream_id, &a.host, &state.measured, &state.play_log);
    let kb = source_rank_key(b.stream_id, &b.host, &state.measured, &state.play_log);
    kb.cmp(&ka)
});
```

(Note: `Candidate` will need a `stream_id` field — currently it's just `url` + `host`. Either derive it from the URL with the existing `stream_id_of_url`, or thread it through from the source loop. Threading is cleaner.)

Rationale: (1) measured > unmeasured; (2) history before theory; (3) HDR ahead of bpp (OLED priority); (4) bpp ahead of raw pixels (starved 1080p < well-fed 720p); (5) pixels as in-bucket tiebreaker; (6) codec / fps as final tiebreakers.

### 8. Diagnostic endpoint

`GET /admin/measured-quality` — returns the full sample buffer per key plus the aggregate:

```json
[
  { "stream_id": 12345, "host": "http://cf.example",
    "samples": [ { "at": "...", "source": "PerPlay", "width": 1920, "height": 1080,
                   "codec": "h264", "framerate": 50.0, "pix_fmt": "yuv420p",
                   "color_transfer": "bt709", "bitrate_kbps": 4523 }, ... ],
    "aggregate": { "width": 1920, "height": 1080, "bitrate_kbps_p50": 4480, ... } }
]
```

No write endpoint.

### 9. Persistence and warm start

- `server/data/measured_quality.json`, gitignored, mounted via `docker-compose.yml`.
- Loaded into `AppState` **before** the bootstrap sweep spawns (so the sweep correctly skips already-known keys).
- Atomic flush task watches `dirty`, tempfile → rename on 5 s debounce. Persists the full sample buffer.

## Files to modify

| File | Change |
|---|---|
| `server/src/measured.rs` (new) | `Sample`, `MeasuredEntry` (cap-5 ring), `MeasuredStore` keyed `(u64, String)`, JSON persistence with atomic flush. `PerPlayAccumulator` (in-progress map + inactivity-timer committer). |
| `server/src/probe.rs` (new) | `measure_once()` (manifest fetch → placeholder skip → first segment fetch → classify → plausibility → Sample). `is_placeholder_manifest`. Bootstrap sweep loop. |
| `server/src/codec.rs` | `Classification` gains width/height/framerate/pix_fmt/color_transfer. H.264 SPS parser; HEVC SPS parser. `codec_string()` helper. |
| `server/src/state.rs` | Add `measured: Arc<MeasuredStore>`, `per_play: Arc<PerPlayAccumulator>`, `active_plays: Arc<AtomicUsize>`, `xtream_user_info` for `max_connections`. |
| `server/src/main.rs` | Load store; flush task; bootstrap sweep task; per-play committer task. |
| `server/src/proxy.rs` | (a) `Candidate` gains `stream_id`. (b) `build_candidates`: rank-key sort using `MeasuredStore::get` + `success_score`. (c) `fetch_and_rewrite_playlist`: placeholder detection → `anyhow::bail!` (existing `!probe_request` gate handles mutation). (d) `SegmentToken` gains `d` and `h`. (e) `handle_ts_segment` + segment hot-path: `per_play.note_*` calls gated on `!segment.probe`. (f) `active_plays` guard around `play_playlist`. `TV_DECODES_HEVC_MAIN10` constant beside the rank-key code. |
| `server/src/blacklist.rs` | Add `clear_last_known_good()`. |
| `server/src/api.rs` | `GET /admin/measured-quality`. |
| `server/src/play_log.rs` | No changes; `success_score` derives from the existing snapshot. |
| `docker-compose.yml` | Mount `./server/data`. |
| `.gitignore` | `server/data/`. |

No new Rust deps. **No `Dockerfile` changes** — `ffmpeg` is not used.

## Verification

### Pre-flight

```bash
curl -s "$STREAM_URL" | head -20            # #EXTINF only, not #EXT-X-STREAM-INF
# Reproduce the placeholder: saturate the slot, curl a different stream, expect
# #EXT-X-ENDLIST + ≤2 #EXTINF.
# After first sweep: find a 10-bit HEVC source in /admin/measured-quality;
# attempt TV playback; if it fails, flip TV_DECODES_HEVC_MAIN10 to false.
```

### Unit / local

- `cargo test`:
  - `MeasuredEntry::push` evicts oldest at `WINDOW=5`.
  - `MeasuredEntry::aggregate`: 4 samples ≈4000 kbps + 1 at 50 → p50 near 4000.
  - `is_placeholder_manifest`: positive (real fixture) + negative (long m3u8).
  - `probe::measure_once`: synthetic upstream (mock HTTP) returning placeholder → `None`; returning real m3u8 + TS chunk → `Some(Sample)` with the expected fields.
  - SPS-NAL parse: synthetic 1920×1080 H.264 chunk → `width=1920, height=1080, pix_fmt="yuv420p"`. HEVC chunk → same. 10-bit HEVC chunk → `pix_fmt="yuv420p10le"`.
  - `bpp_bucket` boundaries.
  - `source_rank_key`:
    - 1080p / success 0.2 vs 720p / success 0.9 → 720p ranks first.
    - With `TV_DECODES_HEVC_MAIN10 = false`, 10-bit HEVC HDR loses its HDR rank to an 8-bit H.264 SDR sibling.
- `docker compose up --build`; logs should show no `ffmpeg`/`ffprobe` invocations (it's not installed any more).

### Bootstrap sweep

```bash
docker logs iptv-proxy 2>&1 | grep -E 'sweep|placeholder|measure_once' | tail -30
```

- Probe starts at ~2/s (or ~1/s with `max_connections=1`).
- Placeholder skips logged for auth-saturated cases.
- After ~15 min: `curl http://localhost:8080/admin/measured-quality | jq 'length'` grows toward TV source × host count.
- `curl http://localhost:8080/admin/measured-quality | jq '[.[] | select(.aggregate.height >= 1080)] | length'` is plausible.
- No radio entries are expected: sweep skips `ChannelKind::Radio` (pre-flight 4 confirmed master playlists + raw AAC). Verify with `jq '[.[] | select(.aggregate.height == 0)] | length' # → 0`.

### Per-play extraction (no separate measurement fetch)

```bash
docker logs -f iptv-proxy 2>&1 | grep -E 'classified|measure_once' &
# Play a channel.
# Expect: one "classified stream" log line; ZERO measure_once invocations
# (sweep doesn't fire for a key that already has samples).
```

After the play settles for ~30 s:

```bash
curl http://localhost:8080/admin/measured-quality | jq \
    '.[] | select(.stream_id==<id> and .host=="<host>") | .samples[-1]'
# Expect a fresh PerPlay sample with non-null codec, pix_fmt, color_transfer,
# bitrate_kbps. The committer task fired on inactivity.
```

### success_score in action

```bash
# Kill a backend host (iptables drop / point the config entry at a bogus name).
# Trigger a few plays through that host.
# Inspect /admin/recent-plays: confirm Err/Timeout attempts logged for that host.
curl -X POST http://localhost:8080/admin/clear-blacklist
# Play the channel and check x-upstream header — the chosen URL must now be on
# the working host, not the failing one.
curl -sI http://localhost:8080/play/<channel-key>.m3u8 | grep -i x-upstream
```

### Placeholder defence end-to-end

- Saturate the provider (TV plays, `max_connections=1`).
- From the laptop: `curl -i http://localhost:8080/play/<other-key>.m3u8`.
- **Before:** 200 + placeholder m3u8.
- **After:** 502/503; `/admin/recent-plays` shows `AttemptOutcome::Err { reason: "upstream returned placeholder…" }`.

### Steady-state: no sweep traffic after ~30 min mixed plays

```bash
docker logs --since 5m iptv-proxy 2>&1 | grep -c 'measure_once'
# Expect ~0 once watched channels each have ≥1 PerPlay sample.
```

### LKG reset

- Before the bootstrap sweep finishes: play channel X; note the upstream URL.
- Wait for the sweep complete + `clear_last_known_good` log line.
- Play X again. If measured ranking disagrees with the prior LKG, the upstream URL must differ.

### Probe-mode segments don't poison the cache

`/api/probe/{video,audio}.m3u8` redirects to `/play/<top>?probe=1`. The client capability probe (caps.js on first boot) drives these. Verify:

- After a probe runs, `/admin/measured-quality` shows NO new sample for the probed channel (the `!segment.probe` gate suppresses both `note_classification` and `note_segment_kbps`).
- `/admin/recent-plays` shows no probe events in the log (the probe path skips `play_log.record`).

### Integration on `utilities`

```bash
ssh utilities "cd /root/projects/iptv && git pull && docker compose up -d --build"
ssh utilities "docker logs iptv-proxy 2>&1 | grep -E 'sweep|placeholder|classified' | tail -50"
curl https://iptv.mglopes.com/admin/measured-quality | jq 'length'
curl https://iptv.mglopes.com/admin/recent-plays | jq '[.[] | select(.succeeded==false)] | length'
```

### Regression checks

- Single-source channels still play.
- Radio plays still work; ranking is a no-op (one source per channel) and no measurement infrastructure touches radio (sweep skip, per-play URL-pattern mismatch).
- Sweep failure on one URL doesn't kill the loop (each measure_once in its own spawn).
- Sweep yields to user plays (active_plays gate verified).
- Concurrent plays don't trigger duplicate per-play samples (classifier cache dedupes, accumulator keys on `(stream_id, host)`).
- `max_connections` is respected (sweep concurrency caps per the table).
- The client capability probe (`/api/probe/*`) still works and STILL doesn't write to the cache (probe-mode segment guards).

## Not in scope (stretch goals)

- Master-playlist sampling (if pre-flight 1 reveals master playlists).
- Audio quality signals beyond bitrate (channel layout, sample rate).
- Display-aware HDR ranking (different rank when served to a browser vs TV).
- HEVC main10 auto-detect (today: manual pre-flight + code constant; a "play once and watch for canplay" runtime check could flip the flag automatically).
- Per-channel sample buffer sizing (cap=5 is global).
- Radio-specific signals (ADTS sample rate, channel count) extracted from PMT audio PIDs.
