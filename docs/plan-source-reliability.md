# Source reliability: no-discard discipline + strict capability

## Spirit

> Links never fail. We always offer the best. We are sure what the client can play. We are sure the stream is up. The user gets a **seamless experience even if the server does more work** to make it so.

Every rule below serves this. Where two rules conflict, prefer the one that pushes work onto the server and away from the user: pre-validate manifests instead of letting the client see a broken playlist; pre-warm capability inference instead of failing once in production; pre-rank measured-quality on idle CPU so play-time sort is a no-op. The proxy is a small process on a server with idle headroom; the TV is a fragile demuxer that the user is watching. The asymmetry should be exploited.

## Goals

1. **Links never fail.** Every URL the server hands the client has passed end-to-end validation recently (manifest fetched, placeholder rejected, first segment fetchable, codec compatible with the requesting client). The proxy doesn't gamble — it pre-pays the verification cost in the background so play-time is instant.
2. **We always offer the best.** The top candidate is measurement-driven for every `(source × host)` the client can actually decode. Manual override (§7) exists for the 1% miss.
3. **We are sure what the client can play.** Each client carries a precise capability matrix — probed at boot, narrowed in production by post-canplay failures. The server filters per-request based on what the client *proved* it can decode; it never offers what the client can't play and never hides what the client can.
4. **No source disappears because we *suspect* it's broken.** Only `caps_required ∩ client_caps = ∅` hides a channel. Cool-off, demote, and host-bad are all deep ranking penalties, never exclusions.

## Philosophy

1. **Pre-commit over post-commit.** When the server commits to a URL, the candidate has been validated recently — measured quality is non-stale, last opportunistic-validation pass was ≤ N minutes ago, placeholder check passed. The validation runs in the background, on the upstream's idle slots, not in the user's play-budget.

2. **Capability is ground truth, not heuristic.** The client tells the server what it can play; the server filters on set inclusion. Failures grow the client's local "no longer plays X" set, not a server-side blacklist. (Per user constraint: no per-client server state.)

3. **Server-global signals are objective only.** Upstream 5xx, connection timeout, placeholder manifest, abuse redirect — facts about the upstream, identical for every client. They feed server-side cool-off (a soft rank penalty).

4. **Subjective failures stay subjective.** Post-canplay HLS decoder errors, mid-stream stalls — those are client-decoder-specific. They feed the client's local cap set, not the global pool.

5. **Don't discard, ever.** Every `(source × host)` stays in the candidate list, ranked. The only thing that hides a source from a client is `caps_required` ∩ `client_caps = ∅` — and even then, only at the channel-list level (the channel disappears from the UI, not the source from the candidate list of a visible channel).

6. **Spend the proxy.** If a background pass at twice the rate keeps the measured cache warm, double it. If pre-validating ten channels per heartbeat tick reduces play-time misses by half, do it. The proxy has CPU and bandwidth budgets we are not exhausting.

## What's already in the tree

This plan rides on existing work:

- **Measured-quality system** (`server/src/measured.rs`, `probe.rs`, `codec.rs::classify_ts_chunk` with SPS extraction). Key invariants this plan rides on:
  - `MeasuredStore` keyed `(stream_id: u64, host: String)`, 5-sample rolling buffer per key.
  - `Sample` shape: `{at, source: Sweep|PerPlay, width, height, codec, pix_fmt, color_transfer, framerate, bitrate_kbps}` (extended below by Steps 7 and 10).
  - Aggregate: most-recent for stable fields; median for bitrate.
  - Bootstrap sweep walks alive hosts once at startup, skipping already-sampled keys; per-play accumulator commits at 30 s quiesce.
  - Placeholder defence (`is_placeholder_manifest`) detects auth-saturated `#EXT-X-ENDLIST` + ≤2 `#EXTINF`.
  - Rank-key in `proxy.rs::source_rank_key` is currently 8-element `(measured?, success, hdr, bpp, pixels, codec, fps, kbps)`; this plan extends it.
- **Per-play upstream attribution** (`server/src/play_sessions.rs`, `pid` query param round-tripping through `play_playlist` → client's `state.playing.playId` → `/api/feedback/:key` body). The plumbing that lets the server know exactly which upstream a client was playing.
- **Client capability probe** (`app/js/caps.js`, `server/src/api.rs::parse_client_caps`, `caps_required`). Twelve tags probed at boot, set-inclusion filter on `/api/channels`.
- **Opportunistic post-play validation** (`server/src/proxy.rs::schedule_opportunistic_validation`). Already validates two other candidates per successful play; we strengthen its role rather than replace it.
- **Active-play guard** (`server/src/proxy.rs::ActivePlayGuard`, `state.active_plays`). The sweep yields when users are watching; same gating works for the new pre-warming pass.

## Non-goals

- Per-client server state. No per-client blacklist, LKG, or capability cache. Per-request headers (`X-Client-Caps`) are not state.
- Client-driven candidate iteration. Server keeps doing failover internally (§6 stays server-side; doc already updated).
- New measurement infrastructure beyond ADTS for radio. The TS classifier + sample-buffer + sweep are sufficient.
- Catalog dedup, EPG, catch-up.
- `ffprobe` / `ffmpeg`.

## Pre-flight

1. **Confirm measured cache populated.** After ≥ 30 minutes of catalog uptime, `curl /admin/measured-quality | jq 'length'` should be ≥ 70 % of (TV channel × alive_host) pairs. Per-channel codec-specific `caps_required` (§Approach 7) depends on this; sparse coverage falls back to the loose per-`ChannelKind` defaults.

2. **Reproduce a placeholder response.** Saturate the slot (TV playing), then `curl another.m3u8`. Expect the auth-saturated `#EXT-X-ENDLIST + ≤2 #EXTINF` shape. Confirm `probe.rs::is_placeholder_manifest` catches it (already covered by tests).

3. **HEVC main10 decode check on the TV.** After ≥ 30 min uptime, find a 10-bit HEVC source via `curl /admin/measured-quality | jq '.[] | select(.aggregate.pix_fmt | test("10"))' | head`. Attempt playback on the TV; observe `canplay` + frames advancing. If it fails, set `TV_DECODES_HEVC_MAIN10 = false` in `proxy.rs` AND don't include `hevc_main10` in the probe matrix below.

## Approach

### 1. Failure-phase signal (`pre-canplay` / `post-canplay`)

The smallest piece — plumbs a missing distinction end-to-end. Unblocks Steps 2, 4, and 8.

**Files:**

- `app/js/player.js`:
  - `error` handler (line 45): compute `phase = v.readyState >= 2 ? "post-canplay" : "pre-canplay"`. Pass to `onSourceFailed(url, reason, phase)`.
  - Watchdog handler (line 90): always `"pre-canplay"` (the watchdog only fires before `readyState >= 2`).
  - hls.js fatal handler (line 124): same `readyState` check.
- `app/js/api.js`:
  - Extend `reportFailure(key, playId, error, phase)` and `demoteSource(...)`. Include `phase` in POST body.
- `app/js/main.js::onSourceFailed` (line 1565): forward `phase` through.
- `server/src/api.rs::FeedbackBody`: add `phase: Option<String>`. Log it. No behaviour change yet.

### 2. State-machine cool-off with disk persistence

Replaces `blacklist.rs`'s binary world with the §5 state machine.

```rust
struct UrlState {
    cool_off_step: u8,                 // 0..=4 → {none, 1m, 5m, 30m, 6h}
    consecutive_fails: u8,
    last_error_at: Option<SystemTime>,
    last_heartbeat_at: Option<SystemTime>,
}

enum FailureKind {
    ServerSide,        // upstream 5xx, timeout, placeholder — full step + host blame
    ClientPostCanplay, // mid-playback error — full step, no host blame
    ClientPreCanplay,  // slow-to-start (§4: not unstable) — log-only, no state mutation
}
```

**Files:**

- `server/src/blacklist.rs`: replace `Inner::failed_urls` + `Inner::demoted_urls` with `per_url: HashMap<String, UrlState>`. Move from `Instant` to `SystemTime` so it serialises.
  - **Keep** `Inner::host_streams` and `is_host_bad` as-is — they are host-level signals, not per-URL state, and Step 4's `host_penalty` reads them.
  - **Keep** `Inner::last_known_good` (used by Step 5; the post-sort promotion at `proxy.rs:441-466` reads it).
  - **Keep** `note_url_succeeded` (writes `last_known_good`; called from `proxy.rs:238` on successful playlist fetch). Unchanged signature.
  - `note_failure(url, FailureKind)`: behaviour depends on the variant:
    - `FailureKind::ServerSide` (upstream 5xx / connection timeout / placeholder manifest): increments `consecutive_fails`, advances `cool_off_step` by one, stamps `last_error_at`, AND replicates the host-bookkeeping side effect that today's `note_url_failed` does at `blacklist.rs:60-64` — `host_streams.entry(host).insert(stream_id, now)`. Without this, `is_host_bad` stays permanently false and the EPG/probe-redirect/catchup carve-out (Step 4) becomes a no-op.
    - `FailureKind::ClientPostCanplay` (mid-playback decoder error — §4 "unstable"): same as ServerSide except **no host blame** (a TV decoder issue says nothing about whether the host is up). Increments `consecutive_fails`, advances `cool_off_step`, stamps `last_error_at`.
    - `FailureKind::ClientPreCanplay` (slow-to-start): **log-only no-op for cool-off state**. Per `architecture.md` §4, "slow-to-start is not an instability signal" — this variant does NOT advance `cool_off_step`, does NOT stamp `last_error_at`, does NOT touch `host_streams`. The variant exists purely so the feedback API can log the distinction and `/admin/recent-plays` can show it. The clean-play reset window keeps ticking through a slow-to-start blip exactly as if it never happened.
  - `note_heartbeat(url)`: bumps `last_heartbeat_at`. If `now - last_error_at >= clean_play_reset_secs` (default 300 s, per Step 2's config knob below) AND last heartbeat is fresh (< `heartbeat_window_secs`, default 60 s) → reset `cool_off_step = 0`, `consecutive_fails = 0`, clear `last_error_at`. If `last_error_at` is `None` (no failures ever recorded for this URL) → no-op; nothing to reset.
  - `cool_off_penalty(url) -> i32`: returns step (0..=4) for rank-tuple integration.
  - `is_url_failed` survives as a diagnostic getter but **is no longer used to exclude** in `build_candidates` (see Step 4).
  - `BlacklistConfig` shrinks: `url_fail_threshold`, `url_fail_window_secs`, `demote_ttl_secs`, `url_ttl_secs` are all consumed by the binary model and become unused. Replace with the state-machine knobs: cool-off step durations (`cool_off_steps_secs: [u64; 4]` defaulting to `[60, 300, 1800, 21600]`), heartbeat freshness window (`heartbeat_window_secs: u64` default 60), and clean-play threshold (`clean_play_reset_secs: u64` default 300).
- `server/src/state.rs::AppState::new`: load blacklist from `data/blacklist.json` (mirror `MeasuredStore::load_or_empty`).
- `server/src/main.rs`: spawn `blacklist::run_flush_task` alongside the existing `measured::run_flush_task`. 5 s atomic-rename debounce.
- `server/src/blacklist.rs::tests` (the inline `#[cfg(test)] mod tests` at the bottom): the existing suite tests `url_fail_threshold`, `url_fail_window_secs`, `mark_failed`, `note_url_failed`, etc. — all of which are deleted in Step 4. The test module gets a wholesale rewrite around the new state machine: cool-off escalation per `FailureKind`, heartbeat reset behaviour, host-bookkeeping side effect on `ServerSide` only, persistence round-trip via `load_or_empty`. Treat as part of Step 2's PR.

### 3. Clean-play heartbeat

The signal that drives Step 2's reset.

**Files:**

- `server/src/api.rs`: new `POST /api/heartbeat` with body `{ play_id }`. Looks up `(channel, url)` via `play_sessions`, calls `blacklist.note_heartbeat(url)`. Returns `204`. Quiet on missing session (legacy clients).
- `server/src/main.rs`: wire the route.
- `app/js/api.js`: `export function heartbeat(playId)`.
- `app/js/player.js`: on `canplay`, start `setInterval(30_000)` calling the caller-supplied heartbeat fn; clear on stop / teardown / error.
- `app/js/main.js`: `player.onPlaying` already wires up at ~line 1540 — extend to register the heartbeat interval; `player.stop` paths clear it.

**Cadence:** 30 s heartbeats. Fresh-heartbeat window: 60 s (tolerates one missed tick). Cumulative — pauses don't reset, only errors do (per resolved ambiguity).

### 4. Strict §8: exclusion → rank penalty (ships atomically with Step 5)

The core of "no source disappears because we *suspect* it's broken." Once Steps 1-3 land, `blacklist.rs` has meaningful state but stops being a filter — it becomes a rank-tuple input.

**Scope of §8 enforcement.** "No exclusion" applies to **live-play candidate selection** (`build_candidates` in `proxy.rs`). EPG candidate filtering (`api.rs:260` in `get_epg`), probe-redirect candidate filtering (`api.rs:607,611` in `channel_has_alive_source`), and catchup archive-host selection (`proxy.rs::archive_candidate_hosts` ~ line 1780) keep their existing `is_host_bad` / `is_url_failed` checks — those paths aren't user-visible playback and have different reliability requirements.

**Must ship atomically with Step 5.** Step 4 deletes the `fresh`/`demoted` partition; Step 5 deletes the LKG promotion block at `proxy.rs:441-466`. Shipping Step 4 alone leaves LKG unconditionally promoting (including a cool-off-step-4 source) because Step 4's deletion of `is_url_demoted` short-circuits the existing competitive guard. Treat 4+5 as a single PR.

**Files:**

- `server/src/proxy.rs::build_candidates` (line 354):
  - Delete `is_url_failed` exclusion (lines 373, 655).
  - Delete `is_host_bad` exclusion in the live-play path (lines 377, 396, 404). Host-badness becomes a rank-tuple penalty.
  - Delete the `fresh`/`demoted` split — `is_url_demoted` is a tuple element, not a partition.
  - Delete the safety-valve fallback at lines 475-508 — unreachable when nothing's excluded.
  - Delete the LKG post-sort promotion at lines 441-466 (formerly Step 5; merged here for atomic ship).

- `server/src/proxy.rs::source_rank_key`: extend tuple with **two new leading elements**, both negated so "less penalty" sorts higher. Per-`ChannelKind` dispatch is done by **two separate helpers**, `source_rank_key_tv` and `source_rank_key_radio` (no enum-wrapped variant — Rust's tuple compare across enum variants is a footgun, and `build_candidates` always knows the channel kind at sort time). `build_candidates` selects the right helper based on `channel.kind` and sorts within a single kind; kinds never mix in one channel's candidate list.

  TV tuple:

  ```rust
  type TvRankKey = (
      i32,   // -cool_off_penalty (0..4 inverted: fresh first)
      i32,   // -host_penalty     (host-bad gets demoted in-tuple)
      i32,   // measured?         (1 if measured)
      i32,   // success_bucket
      i32,   // lkg_bonus          (Step 5, decayed by age)
      i32,   // hdr_rank
      i32,   // bpp_bucket
      i64,   // pixels
      i32,   // codec_rank
      i32,   // fps_rank
      i32,   // raw_kbps
  );
  ```

  Order rationale: cool-off (recency-aware reliability) dominates, but a high cool-off doesn't *remove* — a 6h-cooling source with measured 1080p HEVC still beats an unmeasured fresh one if no fresh measured sibling exists.

  `host_penalty(host, &Blacklist) -> i32`: `if bl.is_host_bad(host) { 2 } else { 0 }`.

- `server/src/proxy.rs::mark_segment_failure` (line 1282): becomes `note_failure(url, ServerSide)`. No more `drop_last_known_good` — LKG is decayed via Step 5.

- `server/src/proxy.rs::handle_ts_segment` DVB-subs block (line 1238-1255): three changes.

  **(a) Demote/blacklist mutation removed.** Today the block calls `demote_url` + `drop_last_known_good_if_matches` when subs collide with PCR. Step 7's `dvb_safe` cap requirement (see below) handles the channel-filter responsibility cleanly; the demote here is redundant.

  **(b) `dvb_unsafe: bool` field added to `codec.rs::Classification`.** Set by `classify_ts_chunk` when subtitle PIDs collide with the PCR PID (i.e., the unstrippable case — what today's code calls `strippable_subtitle_pids().is_empty() && !classification.subtitle_pids.is_empty()`). Populated into `Sample.dvb_unsafe` on **both** code paths so Step 7's filter has data regardless of whether a user has watched the channel:
  - per-play: `proxy.rs::handle_ts_segment` already calls `classify_ts_chunk`; `PerPlayAccumulator::note_classification` carries the `dvb_unsafe` bool through.
  - sweep: `probe.rs::measure_once` already calls `classify_ts_chunk`; populate `Sample.dvb_unsafe = cls.dvb_unsafe` in the `Sample { … }` literal at the function's tail.

  Aggregated into `MeasuredQuality.dvb_unsafe` (most-recent semantics, like other stable fields).

  **(c) `strip_subtitle_pids` becomes per-request, gated on the client lacking `dvb_safe` — per `architecture.md` §9.** Today the proxy strips DVB-subtitle PIDs from the TS byte stream unconditionally when stripping is possible. After this change:

  **Caps must ride the URL, not the header.** Critical plumbing fact: `X-Client-Caps` is set only by `app/js/api.js::http()`, which is bypassed entirely by the playback path. webOS uses `video.src = url` (player.js:130 — native demuxer fetches without any JS wrapper); hls.js uses `hls.loadSource(url)` without an `xhrSetup` hook (player.js:119). Neither sends the header on `/play/...` or `/seg/...` requests. So the caps signal has to ride the play URL itself, not a header, to reach `play_playlist` and (via the segment token) `proxy_segment`.

  Plumbing:
  - **Client side, `app/js/main.js`:** when constructing the play URL, append a caps query parameter. Today the URL is built via `appendPid(channel.play_url, pid)` (main.js:914 / 934). Extend to also append `&caps=<comma-list>` from `caps.loadCaps()`. Optionally truncate to the caps that actually gate transformations (today just `dvb_safe`) to keep the URL short — but the full set is fine.
  - **Server side, `proxy.rs::PlayParams`:** add `caps: Option<String>`. `play_playlist` parses the comma-list once and computes `client_has_dvb_safe: bool`.
  - **`rewrite_playlist` / `proxy_url` / `SegmentToken`:** thread `client_has_dvb_safe` into a new bool field on the token. Token is base64-JSON-encoded, so adding one bool is trivial; existing clients that never include `caps` get `client_has_dvb_safe = false` (legacy default — strip applies, matching today's behaviour).
  - **`proxy_segment` / `handle_ts_segment`:** read `segment.dvb_safe` from the token. If true → verbatim passthrough (no PID strip). If false → existing `strip_subtitle_pids(...)` runs.

  Why bake-at-playlist-rewrite, not parse-at-segment-time: the segment HTTP request has no header path to receive caps from. The playlist response IS the place where the server can encode the caps decision into the per-segment URLs; segment fetches then carry that decision in the token. Side benefits: a client can't change caps mid-stream to bypass anything; the strip decision is consistent for an entire play session.

  Default behaviour for a `dvb_safe`-capable client (one that appends `caps=…,dvb_safe,…` to play URLs) is byte-for-byte passthrough. The strip is a per-request fallback for clients that don't claim the cap, never a default mutation for clients that do.

  **Probe-path correctness.** The capability probe `/api/probe/dvb_safe.m3u8` redirects to a `dvb_unsafe` channel (per Step 7 rule 5's selection). The probe URL has no `caps=` parameter (the client is testing whether it can play; it hasn't claimed dvb_safe yet) → `SegmentToken.dvb_safe = false` → strip path requested. But `codec.rs::strippable_subtitle_pids()` returns an empty vec when any subtitle PID collides with the PCR PID (which is exactly what `dvb_unsafe` means), and `handle_ts_segment` short-circuits to `bytes.to_vec()` when the strip vec is empty. So the dvb_safe probe always sees verbatim bytes via the natural short-circuit — no explicit probe gate needed. **Other probes** (`h264`, `hevc`, `live_video_hls`) may land on channels with strippable DVB subs; those continue to be served stripped as today, preserving the existing probe behaviour.

  How the `dvb_safe` cap is derived (Step 7 rule 5): a channel's `caps_required` adds `dvb_safe` only when *every* measured source for the channel has `dvb_unsafe == true` (PCR-collision, truly unstrippable). Strippable-DVB-sub channels do NOT add the cap — any client can play them, either via verbatim passthrough (if `dvb_safe`) or via per-request strip (if not).

  **Naming note.** The stream-side flag is `dvb_unsafe`; the client-side capability is `dvb_safe`. The inversion is intentional — a `dvb_unsafe` stream requires a `dvb_safe`-capable client — but worth keeping in mind when reading both names in the same file.

- `server/src/api.rs::feedback` (line 432):
  - `FeedbackKind::Fail` with `phase == "post-canplay"` → `note_failure(url, ClientPostCanplay)`.
  - `FeedbackKind::Fail` with `phase == "pre-canplay"` or absent → `note_failure(url, ClientPreCanplay)`.
  - `FeedbackKind::Demote` → one cool-off step bump (user pressed Green).

**What dies (enumerated):**

- `proxy.rs:475-508` — the safety-valve fallback block.
- `proxy.rs:441-466` — the LKG post-sort promotion block.
- `blacklist.rs`:
  - `mark_failed` (replaced by `note_failure(_, ServerSide)`).
  - `note_url_failed` — its sole caller `mark_failed` dies, and the function is otherwise test-only. Either delete entirely or keep as `#[cfg(test)]` if any test still exercises it.
  - `demote_url`, `is_url_demoted`, `snapshot_demoted`, `clear_demoted` (no more demote bucket).
  - `drop_last_known_good`, `drop_last_known_good_if_matches`, `clear_last_known_good` (LKG decays via Step 5; no manual clear path).
  - `clear_blacklist`, `clear_all` (admin clearance becomes meaningless once cool-off is a soft penalty).
- `api.rs:475-489`: `admin_clear_blacklist`, `admin_clear_demoted`, `admin_clear_all` route handlers; corresponding routes in `main.rs:110-113`.
- `api.rs:168-172`: `BlacklistStatusDto.{failed_urls, demoted_urls}` and the `state.blacklist.snapshot_urls() / snapshot_demoted()` reads (line 341-343) that feed them. **Keep** `BlacklistStatusDto.bad_hosts` and `snapshot_hosts` (`api.rs:305`) — `is_host_bad` survives (Step 2 keeps `host_streams`). Replace `failed_urls + demoted_urls` with a single `url_states_count: usize` derived from the new `per_url` map size.
- `BlacklistConfig` fields: `url_fail_threshold`, `url_fail_window_secs`, `demote_ttl_secs`, `url_ttl_secs` — replaced by the state-machine knobs (Step 2).
- `app/js/main.js` and `app/js/api.js`: any UI affordances tied to the deleted admin endpoints (clear-blacklist / clear-demoted buttons in the admin panel).

**Also update `docs/architecture.md`:** delete the safety-valve sentence in §8 line 51 (the valve is unreachable after this step). Replace with a sentence stating that all failure signals feed rank-key penalties; nothing is excluded except by `caps_required`.

### 5. LKG as decayed bias in the rank tuple (ships with Step 4)

Resolves the §2 "bias" wording. **Bundled into the Step 4 PR** because shipping Step 4 alone leaves the LKG promotion block live and broken (see Step 4's atomicity note).

**Files:**

- `server/src/blacklist.rs`: add `last_known_good_age(channel_key, url) -> Option<Duration>`. (Dead-code removal of `clear_last_known_good`, `drop_last_known_good`, `drop_last_known_good_if_matches` is in Step 4's "What dies" list.)
- `server/src/proxy.rs::source_rank_key`: add `lkg_bonus` (the slot is already reserved in Step 4's TV tuple):

  ```rust
  fn lkg_bonus(channel_key: &str, url: &str, bl: &Blacklist) -> i32 {
      match bl.last_known_good_age(channel_key, url) {
          Some(age) if age < Duration::from_secs(3600)        => 3, // < 1h
          Some(age) if age < Duration::from_secs(6 * 3600)    => 2, // < 6h
          Some(age) if age < Duration::from_secs(24 * 3600)   => 1, // < 24h
          _ => 0,
      }
  }
  ```

  Slot position (already in the TV tuple): after `success_bucket`, before `hdr_rank`. LKG beats quality among rank-equal-on-success siblings, but a measurement that says "this sibling is actually better" still wins. Critically, `cool_off_penalty` and `host_penalty` come *before* LKG in the tuple — a freshly-broken LKG is still demoted by its cool-off step, so the spirit "links never fail" survives even for the previously-good URL.

- `server/src/probe.rs::run_bootstrap_sweep` (line 265): delete `state.blacklist.clear_last_known_good()` — no longer needed.

### 6. Reinforced pre-validation (the "seamless" bet)

Today `schedule_opportunistic_validation` (proxy.rs:1393-1523) runs *after* a successful play, on a few candidates. Strengthen it into a continuous freshness pass.

**Files:**

- `server/src/probe.rs`: new `pub async fn run_freshness_loop(state: Arc<AppState>)`. Walks every (channel × source × alive_host) every N minutes (default 15). For each:
  - If `measured.has_samples(...)` AND most-recent sample is < freshness_ttl ago → skip.
  - Otherwise, run `measure_once` (existing function). This adds a fresh `Sample::Sweep` to the buffer — keeps the cache warm and refreshes per-host availability.
  - Yields to `state.active_plays > 0` exactly like the bootstrap sweep.
  - Concurrency cap is `max_connections`-gated — see the rollout-default section below for the canonical formula.
- `server/src/main.rs`: spawn the freshness loop after the bootstrap sweep completes.
- `server/src/proxy.rs::schedule_opportunistic_validation`: keep as-is for now (post-play targeted validation is complementary — it validates the candidates the user is *likely to need next*). Change its `mark_failed` call sites to `note_failure(url, ServerSide)` so it feeds cool-off cleanly instead of poisoning the binary blacklist.

This is the "more work for seamless" trade. The sweep stops being one-shot and becomes the engine that keeps "we are sure the stream is up" true.

**Tuning knobs in config.rs:**

```rust
pub freshness_loop_interval_secs: Option<u64>,  // None = auto, Some(0) = force-off, Some(n>0) = force-on at n
pub freshness_ttl_secs: u64,                    // default 3600 (1h before re-probe)
```

**Rollout default is `max_connections`-gated, not unconditionally OFF.** Spirit point 6 says "Spend the proxy" — *use idle resources*. With `max_connections = 1`, there ARE no idle upstream slots; every probe collides with the user's play. So the freshness loop only delivers the seamless promise when the upstream allows it. With `freshness_loop_interval_secs = None` (the default) the behaviour at startup is:

- `max_connections == 0` (provider hasn't reported yet): loop sleeps until probe round populates the value.
- `max_connections == 1`: loop stays OFF. (One slot for the user; nothing left.)
- `max_connections == 2`: loop stays OFF by default. (One reserve slot is too tight under bursts.)
- `max_connections >= 3`: loop runs at `interval_secs = 900` (15 min). Concurrency cap: `max_connections.saturating_sub(active_plays + 1).min(2)` — leaves one slot for the user always.

`Some(0)` force-disables (escape hatch for incident response); `Some(n)` for any `n > 0` force-enables at that interval regardless of `max_connections`. The `Option` shape avoids the magic-u32-sentinel footgun.

The Spirit is delivered the moment the upstream supports it; the plan is honest about the per-provider gating instead of hiding behind a flat default.

### 7. Per-channel `caps_required`

Tightens §3's filter so it actually hides what the client can't play. Today `caps_required` returns a per-`ChannelKind` constant; should return per-channel based on measured codec.

**Files:**

- `server/src/api.rs::caps_required`: **breaking signature change** — was `fn(ChannelKind) -> &'static [&'static str]`, becomes:

  ```rust
  fn caps_required(channel: &CanonicalChannel, measured: &MeasuredStore) -> Vec<&'static str>;
  ```

  Callsites that need updating: `api.rs:194` (list_channels filter), `api.rs:221` (ChannelDto field). `ChannelDto.caps_required` becomes `Vec<&'static str>` (still serialises identically — JSON array of strings — so the client sees no schema change).

  Algorithm:
  1. Start with the per-kind baseline (`["hls", "h264", "aac", "live_video_hls"]` for TV).
  2. For each `(stream_id, host)` source: look up `measured.get(...)`. Collect the set of measured codecs / pix_fmts the channel actually offers.
  3. If *every* measured source uses HEVC → replace `h264` with `hevc`.
  4. If *every* measured source has `codec == "hevc" && pix_fmt.contains("10")` → add `hevc_main10`. (The pix_fmt check is what makes this main10-specific; 10-bit H.264 exists in theory but isn't a thing for our provider's TV catalog, and rule 3 already covers any HEVC.)
  5. If *every* measured source has `dvb_unsafe == true` (`MeasuredQuality.dvb_unsafe`, propagated from Step 4's classifier) → add `dvb_safe`.
  6. If `measured` has no samples for any source of the channel → return the baseline (no tightening — don't hide channels we haven't measured).

  Caches the result per channel; recompute on catalog refresh or measured-quality update (debounce via `dirty` flag on `MeasuredStore`).

- **Cap-matrix versioning** (cold-start cache invalidation): on every `/api/channels` response, include header `X-Caps-Matrix-Version: <hash>` where `<hash>` is a stable digest of `(catalog_channels_count, sorted list of (probe_endpoint, picked_channel_key) tuples)`. The client stores the last version it probed against; if the version changes, `caps.js::ensureCaps` clears the localStorage cache and re-probes before sending the next `/api/channels` request.

  Without this, the freshness loop (Step 6) populating measured samples can cause server-side `caps_required` to tighten while a client still advertises looser cached caps — channels would silently disappear without the client re-probing to learn it can play them.

  Storage: server computes the hash once per catalog refresh + each time `MeasuredStore::dirty` flips (debounced). Cheap; the per-channel cap cache already exists. Client persists `xtream.caps.matrix_version` in localStorage alongside the existing `xtream.client.caps.v2`.

- `server/src/api.rs::list_channels` (line 175): pass `&state.measured` into the filter; emit the version header.

- `server/src/api.rs`: new probe-matrix routes that pick representative channels by measured codec/pix_fmt:

  ```
  /api/probe/h264.m3u8           → probe_h264
  /api/probe/hevc.m3u8           → probe_hevc
  /api/probe/hevc_main10.m3u8    → probe_hevc_main10
  /api/probe/av1.m3u8            → probe_av1  (future)
  /api/probe/dvb_safe.m3u8       → probe_dvb_safe
  ```

  Each `probe_*` filters `state.catalog.snapshot().channels` to those whose measured codec/pix_fmt matches, picks the highest-curation-ranked one, and redirects to `/play/<key>?probe=1`. Returns 404 if no matching channel exists (the cap stays false; the client doesn't advertise that cap).

- `app/js/caps.js::PROBES`: append per-codec probes. Each is a `playProbe(...)` call against the new endpoint. Also: `ensureCaps` checks `localStorage.xtream.caps.matrix_version` against the version-header from the last `/api/channels` response; on mismatch, clear and re-probe.

### 8. Client-side cap inference from post-canplay failures

Closes the loop: production failures shrink the client's local cap set, so the next request to `/api/channels` filters out channels the client just proved it can't play.

**Files:**

- `app/js/caps.js`:
  - New `markCapFailure(tag)` / `markCapSuccess(tag)`. Persist counters per-cap in `localStorage` (`xtream.caps.recent.<tag>` = `{fails, last_fail, successes, last_success}`).
  - Eviction rule: if `fails >= 3` AND `successes == 0` AND `last_fail < 24h ago` → drop the cap from `loadCaps()`.
  - Cool-off: a single `markCapSuccess` resets `fails` and clears the eviction.
- `app/js/main.js::onSourceFailed` (line 1565): when `phase === "post-canplay"`:
  - Read `state.playing.channel.caps_required` (already in the ChannelDto).
  - For each cap that's *specific* (not `hls`/`aac`/`live_*` — those are universal) → `caps.markCapFailure(tag)`.
  - If any cap was evicted → call `setClientCaps(caps.loadCaps())` and refresh `/api/channels` (server will now hide that channel + siblings).
- `app/js/main.js::player.onPlaying` (line ~1540): when `canplay` fires → `markCapSuccess` for each specific cap in the channel's `caps_required`.

**Why this matters for "seamless":** the second time the user hits a HEVC-main10 channel that this TV's decoder chokes on, the channel is already gone. They never see it fail twice.

**Caveat — only effective for homogeneous-codec channels. The Spirit promise "never see it fail twice" only holds homogeneously; for mixed-codec channels, the user WILL see exactly one post-canplay failure before defence-in-depth kicks in.** Step 7's `caps_required` tightens only when *every* measured source for a channel matches a given codec. Mixed-codec channels (some sources HEVC, some H.264) don't get a `hevc` cap requirement, so a client without HEVC can still be served the HEVC source for that channel — the failure manifests, the client reports it via `phase: "post-canplay"`, and only then does server-side cool-off deprioritise the HEVC source in the rank for everyone. The cap-eviction loop is the cleaner "never even one failure" mechanism but only fires for the homogeneous case.

Closing the mixed-codec gap fully would require either per-source (not per-channel) caps, or pre-commit decoder-shape compatibility check (server verifies client_caps against the candidate's measured codec before committing). Both are deferred — see "Not in scope" below.

### 9. User override (§7)

Left/right cycles candidates while playing. Steps 4 + 5 mean the candidate list is real and complete; this step makes it accessible.

**Files:**

- `server/src/api.rs`: new `GET /api/candidates/:key`. Returns the ranked candidate list (same as `build_candidates` output) plus the measured-quality summary per row.

  **Deployment scope assumption.** The endpoint exposes raw upstream provider URLs in its response, same risk surface as today's `/admin/recent-plays`, `/admin/measured-quality`, and `/admin/clear-blacklist` — all of which are unauthenticated. This plan inherits the project's existing single-tenant assumption (one user, deployed behind a reverse-proxy on a private LAN). If the deployment ever changes (multi-tenant, public exposure), `/api/candidates` + the existing `/admin/*` endpoints all need an auth gate together; the plan doesn't address that scope.

  Response shape:

  ```json
  [
    { "url": "http://cf.host-a/.../12345.m3u8",
      "host": "http://cf.host-a", "stream_id": 12345,
      "measured": { "width": 1920, "height": 1080, "codec": "h264",
                    "bitrate_kbps": 4523, "pix_fmt": "yuv420p" },
      "rank_pos": 0, "cool_off_step": 0, "lkg_age_secs": 1240 },
    ...
  ]
  ```

  Read-only.

- `server/src/proxy.rs::play_playlist`: accept `?force_url=<base64url>` for a single forced candidate.
  - Validate against the **current** `build_candidates` output (the call is freshly made inside `play_playlist`, so catalog refreshes between the client's `/api/candidates` fetch and the eventual `/play/...?force_url=...` request are naturally honored — a URL that's no longer a candidate is rejected even if the client cached it).
  - Reject with 404 if the decoded URL is not in the current candidate set (security — don't let a hostile client proxy arbitrary URLs).
  - Promote that candidate to position 0 for this play only. No state mutation.

- `app/js/api.js`: `forceCandidate(key, url)` builds the play URL with `?force_url=...`. **All play-URL construction (normal play in main.js:914/934, forced play here, future paths) must go through a single helper that appends `&pid=…&caps=…` consistently** — otherwise a force-play would silently strip for a `dvb_safe` client (bypasses §9). Extract a `buildPlayUrl(baseUrl, { pid, caps, force_url? })` in `app/js/api.js` and use it everywhere a play URL is generated.

- `app/js/main.js`:
  - New `toggleCandidateOverlay()` bound to a chord (e.g., long-press blue, or red+ok). Displays the candidate list inline.
  - While the overlay is open, left/right select; OK commits → `forceCandidate(...)` → replay with fresh pid.
  - Outside the overlay, left/right keeps current behaviour (`moveHorizontal` for catchup-seek / panel-switch).

### 10. Radio ADTS extraction

Brings radio into the measurement system so §2's audio-quality ranking becomes real.

**Files:**

- `server/src/adts.rs` (new): parse raw AAC ADTS headers. Extract `sampling_frequency_index`, `channel_configuration`. Compute kbps from `bytes_read / segment_duration`.
- `server/src/codec.rs`: new `pub fn classify_aac_chunk(bytes: &[u8]) -> Option<AudioClassification>`. Parallel to `classify_ts_chunk`.
- `server/src/measured.rs::Sample`: extend with optional `sample_rate_hz: Option<u32>`, `audio_channels: Option<u8>`. Backwards-compatible (JSON missing fields → `None`).
- `server/src/probe.rs`: split `measure_once` into per-kind helpers — `measure_once_tv` (today's TS flow) and a new `measure_once_audio` (fetch master → resolve to chunklist → fetch one ADTS segment → `classify_aac_chunk` → build `Sample`). `run_bootstrap_sweep` (line 208) stops skipping `ChannelKind::Radio`; dispatches on `channel.kind` to call the right helper. Without splitting, the radio sweep just falls into the TS classifier and produces no samples — Step 10 would land dead.
- `server/src/proxy.rs::source_rank_key`: per Step 4's per-kind dispatch, add `RadioRankKey` variant:

  ```rust
  type RadioRankKey = (
      i32,   // -cool_off_penalty
      i32,   // -host_penalty
      i32,   // measured?
      i32,   // success_bucket
      i32,   // lkg_bonus
      i32,   // kbps_bucket  (audio: 320+/192+/128+/64+/below)
      i32,   // sample_rate_bucket (48k/44.1k/32k/below)
      i32,   // channels (2/1/0)
  );
  ```

  Sort within a channel's candidate list only mixes one `ChannelKind`, so the per-kind tuples don't need a unified comparison — `build_candidates` dispatches on `channel.kind`. Bitrate dominates; codec is implicit (AAC LC vs HE-AAC could be a tiebreaker once we parse the profile byte).

## Files touched (summary)

| File | Touched by step | Change |
|---|---|---|
| `app/js/player.js` | 1, 3 | `phase` arg to callback; canplay-triggered heartbeat |
| `app/js/api.js` | 1, 3, 9 | `phase` field; `heartbeat`; `forceCandidate` |
| `app/js/caps.js` | 7, 8 | new probes; `markCapFailure`/`markCapSuccess` + eviction |
| `app/js/main.js` | 1, 3, 4, 8, 9 | forward `phase`; heartbeat wiring; cap-success on play; candidate overlay; append `&caps=…` to play URL alongside `&pid=…` so the §9 transformation decision rides the URL |
| `server/src/blacklist.rs` | 2, 4, 5 | state-machine rewrite; persistence; new getters |
| `server/src/state.rs` | 2 | blacklist load-or-empty path |
| `server/src/main.rs` | 2, 3, 6 | flush task, heartbeat route, freshness loop |
| `server/src/api.rs` | 1, 3, 7, 9 | `phase`; heartbeat route; per-channel caps; probe matrix; `/api/candidates` |
| `server/src/proxy.rs` | 4, 5, 6, 9 | exclusion → penalty; LKG into tuple; `?force_url`; opportunistic feeds cool-off; DVB-strip becomes per-request (gated on `dvb_safe` cap) per §9 |
| `server/src/probe.rs` | 6, 10 | freshness loop; don't skip radio |
| `server/src/codec.rs` | 10 | AAC classifier |
| `server/src/adts.rs` (new) | 10 | ADTS parser |
| `server/src/measured.rs` | 10 | audio fields on `Sample` |
| `server/src/config.rs` | 6 | freshness-loop config |
| `docker-compose.yml` | 2 | mount confirms `server/data` (already mounted for measured-quality) |
| `.gitignore` | 2 | `server/data/blacklist.json` (umbrella `server/data/` already present) |
| `tests/e2e/*.spec.ts` (new) | all | Playwright spec per step (`step-01-failure-phase.spec.ts`, etc.) |
| `tests/e2e/playwright.config.ts` (new) | 1 | bootstrap Playwright config (introduced with Step 1's spec) |
| `scripts/tv-eval.md` (new) | all | TV manual-eval checklist, one section per step |
| `scripts/tv-eval` (existing stub) | 1 | repurpose to print the checklist + open it in $EDITOR |
| `package.json` (new or existing) | 1 | add `@playwright/test` dev dependency, `npm test` script |

No new Rust deps. One new client dev dep (`@playwright/test`) — runtime client deps unchanged.

## Verification

### Per-step signal

- **Step 1:** `/admin/recent-plays` shows attempts with `phase` populated in the error log entry.
- **Step 2:** restart the server; `/admin/status` shows the persisted cool-off state. `curl -X POST /api/feedback/<key>` 5 times → URL escalates step-by-step. Wait 5 minutes with heartbeats firing → step resets to 0.
- **Step 3:** play a channel; tail logs for `heartbeat` events every 30 s. `curl /admin/status` shows `last_heartbeat_at` advancing.
- **Step 4:** every `(source × host)` appears in `/api/candidates/<key>` (no exclusion). Forcing a known-bad source via `?force_url` returns the right answer (server tries it, falls through). A 6h-cooling source is at the bottom of the rank order, not absent.
- **Step 5:** play X, note LKG. Wait 12 h. `/api/candidates/X` shows `lkg_age_secs ~ 43200`. Top candidate is no longer the LKG if a measured-better sibling exists.
- **Step 6:** the freshness loop is observable in logs (`freshness sample` debug). After 30 minutes, every (stream_id × alive_host) pair has a Sample with `at` ≤ freshness_ttl.
- **Step 7:** before — HEVC-only channel listed for an HEVC-incapable client. After — same channel filtered out. `/api/probe/hevc.m3u8` returns 307 to a HEVC channel; the boot probe sets `hevc` cap.
- **Step 8:** after three failed HEVC-main10 plays, `localStorage.getItem('xtream.client.caps.v2')` no longer includes `hevc_main10`. Next `/api/channels` request hides the now-unplayable channels.
- **Step 9:** `curl /api/candidates/<key>` returns the same list `build_candidates` produces. Candidate overlay shows them; left/right + OK commits a force.
- **Step 10:** radio sources appear in `/admin/measured-quality` with `sample_rate_hz` and `audio_channels` populated. Sort within a multi-source radio channel matches expected bitrate order.

### Integration on `utilities`

```bash
ssh utilities "cd /root/projects/iptv && git pull && docker compose up -d --build"
ssh utilities "docker logs iptv-proxy 2>&1 | grep -E 'heartbeat|cool_off|freshness|caps_required' | tail -50"
curl https://iptv.mglopes.com/admin/measured-quality | jq 'length'
curl https://iptv.mglopes.com/api/candidates/rtp1 | jq '. | length'
```

### Regression checks

- Catalog refresh still works (no source-list mutation from the new code paths).
- Catch-up still plays (Step 4's rank-key changes affect live only; catch-up takes a different path).
- Probe-mode segments still don't write to the measured cache or the new cool-off state (the `!segment.probe` gate in `proxy_segment` survives untouched).
- A client without `X-Client-Caps` (older build, curl) still gets the full channel list (`parse_client_caps` returns `None` → permissive).
- A client that drops all its caps (extreme negative inference) still gets the universal-cap channels (the per-kind baseline is always included).

### Automated test coverage

Two test surfaces with strict, asymmetric limits. **Playwright pins signaling; webOS pins decoding. They are NOT redundant — a passing Playwright run says nothing about whether the TV plays HEVC; a passing TV eval says nothing about whether the heartbeat POST body is well-formed. Both are required.**

#### Playwright (laptop / Chromium)

What Playwright CAN exercise:

- §3 capability probe wiring: boot fires `/api/probe/{video,audio,h264,hevc,…}.m3u8`, `caps.js::ensureCaps` resolves to the expected tag set, the next `/api/channels` request sends `X-Client-Caps`. Network-level assertions on the request flow, not on what plays.
- §4 failure-phase signal: trigger `onSourceFailed` via a Playwright route handler that returns 502; assert the POST to `/api/feedback/:key` carries `phase: pre-canplay | post-canplay`.
- §5 heartbeat: play a Chromium-decodable channel (H.264/AAC), assert `POST /api/heartbeat` fires every 30 s. Stop playback, assert heartbeat stops.
- §7 candidate overlay UI: open overlay, navigate left/right, OK, assert the resulting `/play/…?force_url=…&caps=…&pid=…` request shape via `buildPlayUrl`.
- §8 candidate list integrity: after a forced failure, assert `/api/candidates/<key>` still returns every `(source × host)`.
- §9 caps-on-URL: assert play URL contains `&caps=…`; assert resulting `/seg/<token>.ts` tokens decode to `dvb_safe` matching the client's claimed caps.
- §6 freshness loop (when force-enabled via `freshness_loop_interval_secs = Some(n)` in test config): assert `/admin/measured-quality` shows new `Sample.at` timestamps after one loop interval. The "yields-when-active-play" assertion is server-log-based and is left to `tv-eval.md` (line in the §6 section there) rather than wired through Playwright — keeps the test surface honest.
- §7 cap-matrix versioning: hit `/api/channels` twice with intentionally-different measured state between the calls; assert `X-Caps-Matrix-Version` header differs; assert the client's cached `xtream.caps.matrix_version` triggers a re-probe before the second `/api/channels` request.
- §8 client-side cap eviction: route-handler-fake the segment fetches for a HEVC channel to error mid-playback (3 plays). This triggers `player.onSourceFailed → caps.markCapFailure` client-side, which is what actually mutates the cap set. Assert `localStorage['xtream.client.caps.v2']` loses `hevc`. Assert the next `/api/channels` request omits the now-unplayable channel. (Direct POSTs to `/api/feedback` don't exercise this — `markCapFailure` lives in JS, server-side feedback handling doesn't fire it.)
- §9 force_url negative path: request `/play/<key>?force_url=<base64-of-a-URL-NOT-in-the-current-candidate-set>` → assert 404. This is the only thing standing between this endpoint and "open URL proxy".
- §10 radio ADTS (partial — full coverage on TV): play a radio channel via hls.js; assert `/admin/measured-quality` carries `sample_rate_hz` and `audio_channels` for the radio entry afterwards. Doesn't cover webOS-specific master+chunklist quirks (those land in `tv-eval.md`).

**Out of Playwright scope — covered by Rust unit tests instead:**

- Step 2's state-machine escalation (cool-off step transitions, half-step semantics for `ClientPreCanplay`, host-blame conditional on `ServerSide`, persistence round-trip) is best tested in `server/src/blacklist.rs::tests` — Step 2's PR already mandates a wholesale test-suite rewrite. Don't expect Playwright to cover state-machine internals; it tests the wire shape, not the bookkeeping.

What Playwright **CANNOT** exercise — Chromium's media stack is much narrower than webOS's:

- **HEVC decode** (`hevc`, `hevc_main10`, HDR transfer). Chromium ships H.264/VP8/VP9/AV1 but HEVC support is rare and platform-dependent (Mac-only on desktop, license-encumbered elsewhere). The `hevc` / `hevc_main10` probes will reliably fail in Playwright — that's expected, not a regression. But it means the cap-eviction logic for HEVC channels can't be triggered from a Playwright run.
- **DVB-sub-bearing TS streams**. Chromium's media stack doesn't surface DVB sub PIDs at all; the `dvb_safe` probe is meaningless there. Whether the §9 verbatim-vs-strip decision actually produces a playable stream on Chromium is undefined. For content tests, assume strip-mode is required on the laptop.
- **50 fps live HLS, master-with-chunklist nesting, EXT-X-MEDIA-SEQUENCE-tagged audio-only HLS** — Linux Chrome's native HLS demuxer chokes on these (which is why the desktop bundle injects hls.js). hls.js covers most of them, but the layer being tested is then hls.js, not the platform decoder.
- **Hardware acceleration / decode performance**. Playwright is always software; webOS uses the chipset's HW decoder. Performance shapes are wildly different.
- **HDR / 10-bit rendering**. Browsers may parse the metadata but not render it; whether the byte stream carries the expected color_transfer is testable; the rendered output is not.

**Practical reading**: Playwright pins the proxy's signaling and the JS state machine. It exercises perhaps half of the plan in mechanism, much less than half by content variety. The other half lives on the TV.

#### webOS TV (LG B4 chipset)

What webOS CAN exercise that Playwright can't:

- Real HEVC / HEVC main10 decode (pre-flight 3 lives here).
- DVB-sub-bearing TS — the entire §9 motivation. The `dvb_safe` probe's truthfulness, and the verbatim-vs-strip §9 split, are observable only on the TV.
- 50 fps live HLS, master-with-chunklist nesting, audio-only HLS for radio — the broadcast-grade content the catalog actually contains.
- §5 reset → 5-minute clean-play behaviour against a real channel that the TV actually decodes for 5 minutes (Playwright fakes this only with H.264 surrogate content).
- Real-world failover under §4/§6 — slow-to-start vs mid-playback distinction observed with the actual upstream provider, not Playwright route mocks.

Scripting today is `scripts/tv-eval` (a stub in the repo). Expand it into a markdown checkbox file the operator runs against the TV after each ship — one section per step in this plan:

```
# scripts/tv-eval.md — TV eval checklist (run after each plan-step PR)

## After Step 1 (failure-phase signal)
- [ ] Tune to a known-good channel; pull network mid-playback → reconnects;
      check /admin/recent-plays for phase=post-canplay on the failure.
- [ ] Tune to a known-broken channel (cooled-off); first attempt times out →
      check /admin/recent-plays for phase=pre-canplay or absent.

## After Step 2+3 (cool-off + heartbeat)
- [ ] /admin/status shows persisted cool-off state across `docker compose restart`.
- [ ] After 5 min of clean play, the URL's cool_off_step resets to 0 in /admin/status.

## After Step 4+5 (strict §8 + LKG into tuple)
- [ ] Force three failures on the top-rank source via the candidate overlay.
- [ ] Verify the source is still in /api/candidates/<key> but at the bottom.
- [ ] Wait for cool-off decay (or admin-reset); confirm source rises back.

## After Step 7+8 (per-channel caps + client eviction)
- [ ] Find a homogeneous-HEVC channel via /admin/measured-quality.
- [ ] Pre-flight 3: play it; observe canplay. If fails, flip TV_DECODES_HEVC_MAIN10=false.
- [ ] Force three HEVC-main10 post-canplay failures; confirm the channel disappears
      from /api/channels next refresh.

## After Step 6 (freshness loop, only if max_connections >= 3)
- [ ] Force-enable: set `freshness_loop_interval_secs = Some(60)` in config for a
      short run. After ~60 s, /admin/measured-quality shows refreshed `at` timestamps.
- [ ] Start a play; observe the freshness loop yielding (server log:
      "freshness: active_plays>0, yielding").

## After Step 9 (user override + §9 caps)
- [ ] Press candidate-overlay chord; left/right cycles; OK commits.
- [ ] Inspect /play/<key>.m3u8?force_url=…&caps=…&pid=…  — caps comma-list
      contains the TV's actually-probed set.
- [ ] dvb_safe-bearing channel: play normally → smooth.
- [ ] Manually paste a play URL with `&caps=` minus `dvb_safe` into the address bar
      (the debug procedure — no in-app affordance) → strip applies → also smooth.
      (No in-app way to do this by design; manual-URL test only.)

## After Step 10 (radio ADTS)
- [ ] /admin/measured-quality shows sample_rate_hz + audio_channels for radio entries.
- [ ] Multi-source radio (if any): top of /api/candidates is highest-bitrate variant.

## Always-on regression (every ship)
- [ ] Tune through 20 random channels (mix TV + radio); all start within budget,
      no "retrying…" overlay.
- [ ] EPG loads for known-EPG channels.
- [ ] Catchup plays on a recent program.
```

The operator's check + a screenshot/short-clip recorded into a per-ship folder is the durable artefact. There is no webOS automation in this plan; that's deferred until/unless the LG SDK gains a scriptable harness.

#### Coverage table

| Concern | Playwright | webOS TV |
|---|---|---|
| Signaling (headers, query params, feedback bodies, `/api/*` shapes) | ✓ | redundant |
| JS state machine (caps cache, heartbeat interval, candidate overlay) | ✓ | redundant |
| Cap-matrix versioning (`X-Caps-Matrix-Version` re-probe trigger) | ✓ | redundant |
| Client-side cap eviction (3-fails → cap dropped → channel hidden) | ✓ | partial (cap-set inspection only; webOS can't easily inspect localStorage from outside the app) |
| `force_url` validation (404 on non-candidate URL) | ✓ | redundant |
| H.264 / AAC basic playback | ✓ | ✓ |
| HEVC / HEVC main10 decode | ✗ | ✓ — only place |
| DVB subs (strip vs verbatim) | ✗ | ✓ — only place |
| Audio-only HLS (radio) | partial (hls.js path) | ✓ |
| Master → chunklist HLS nesting | partial (hls.js path) | ✓ |
| 50 fps live HLS | partial (hls.js path) | ✓ |
| HW acceleration / decode performance | ✗ | ✓ — only place |
| 5-min clean-play reset under realistic conditions | ✓ with surrogate content | ✓ with real content |

#### Where the tests live

- **Playwright suite**: new directory `tests/e2e/` (no e2e test infra in the repo today). Each step's tests in its own file: `tests/e2e/step-01-failure-phase.spec.ts`, `tests/e2e/step-04-strict-rank.spec.ts`, etc. Run via `npm test` against a local server-side `docker compose up`. The server's `data/measured_quality.json` is mounted from a fixture for deterministic content selection.
- **webOS checklist**: `scripts/tv-eval.md` (markdown) + `scripts/tv-eval` (the existing shell stub, repurposed to print the checklist and open it in `$EDITOR` for the operator to tick off and save to `scripts/tv-eval-runs/<date>.md`).
- **Files-touched (Step 1 onward)**: each step PR adds its own Playwright spec file under `tests/e2e/` and updates the corresponding section in `scripts/tv-eval.md`. CI runs only the Playwright bit; TV eval is operator-driven.

## Order

Ship Steps 1 → 2 → 3 → (4+5 bundled) in sequence. Each strictly unblocks the next:

- Step 1 alone is observable (logs show `phase`).
- Step 2 alone breaks nothing (binary blacklist becomes state-machine but `is_url_failed` still returns the same answers).
- Step 3 wires up heartbeats but doesn't yet alter behaviour (the reset path activates with Step 2 already merged).
- **Steps 4 + 5 ship in a single PR.** Shipping Step 4 without Step 5's LKG-block deletion leaves LKG unconditionally promoting (the `is_url_demoted` guard at proxy.rs:442 short-circuits once the demoted partition is gone). The bundled merge flips exclusion → penalty AND moves LKG into the rank tuple together. **This is the visible-to-user change.** Verify rigorously before merging.
- Step 6 (freshness loop) — ships in its own PR. Default config is `freshness_loop_interval_secs = None` (auto-gated by `max_connections`): OFF when `max_cons ≤ 2`, ON at 15-min interval when `max_cons ≥ 3`. On a `max_cons = 1` provider the loop is dormant by design until the upstream allows more slots; flip to `Some(900)` manually only if you've measured headroom.

After Step 6, the cap matrix:

- Steps 7 and 8 ship together (server-side per-channel caps + client-side inference); they're useless individually.

User-facing tail:

- Step 9 (override) — depends on Step 4 being live so the candidate list is real.
- Step 10 (radio ADTS) — independent; ships when multi-source radio appears.

## Not in scope (stretch goals)

- Client-driven candidate iteration (server returning a list, client timing each). Architecturally a flip; not warranted at current scale.
- Per-channel HDR display targeting (different rank for OLED vs LCD client). Today every client is OLED.
- Sub-stream variant ranking inside an HLS master playlist (we currently treat the master URL as the candidate; variant choice is left to hls.js / webOS).
- Auto-flip of `TV_DECODES_HEVC_MAIN10` based on production observations (today the flag is manual; the cap-eviction loop in Step 8 makes the flag less load-bearing).
- Pre-warming the cap matrix on catalog refresh (today the client probes once at boot; pre-warming would mean the *server* speculatively maintaining the cap matrix per-UA, which violates the no-per-client-state rule).
- **Per-source `caps_required` and pre-commit decoder-shape check** — closing the mixed-codec gap (Step 8 caveat) by either (a) attaching a per-source cap set instead of per-channel, or (b) having `play_playlist` cross-check the candidate's measured codec against the client's `X-Client-Caps` before committing and falling through to the next candidate on mismatch. Both eliminate the "one visible failure" window for mixed-codec channels. Deferred because they're additive over Step 7/8 (not blocked by anything) and the current cool-off plus cap-eviction handles homogeneous-codec catalogs cleanly enough.
