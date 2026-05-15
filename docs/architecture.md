# Architecture

Durable design rules for the app. Today this doc covers **source selection and playback** — how the proxy picks which `(source × host)` URL to serve behind a logical channel, when it gives up on one, and how the client communicates capabilities and failures. Other subsystems (catalog refresh, EPG, catch-up, radio vendoring) accrete sections as they need explicit rules.

The product goal that frames everything below: **one logical channel that just works**. The user never sees "RTP 1 HD" / "RTP 1 FHD" / "MEO: RTP 1" — only the channel. The proxy always offers the best quality the current client can actually play, and never removes a source for trivial reasons.

---

## 1. Channel model
- One logical channel = N variants merged by canonical name. User never sees HD / FHD / MEO / VIP — only the channel.
- A logical channel exposes an ordered candidate list internally: every reachable `(source × host)` pair.

## 2. Source ranking — **measured > labelled**
- Initial rank from name labels (cheap, available immediately).
- Once a source has been played, observed properties (actual resolution, codec, bitrate from SPS / playlist `EXT-X-STREAM-INF` / measured throughput) take precedence over the label.
- **TV mode**: rank by measured video quality.
- **Radio mode**: rank by measured audio quality (bitrate primary, codec tiebreak). User cares about audio fidelity on radio; doesn't care for TV.
- Tiebreak by host latency, then by per-channel LKG bias on this device.
- Filters applied **before** ordering: client capability (§3), cool-off (§5).
- Implementation blueprint lives at `docs/plan-measured-quality.md` — defines `MeasuredStore` keyed `(stream_id, host)` with a 5-sample rolling buffer, per-play accumulator that piggybacks on `handle_ts_segment`'s existing TS classification, bootstrap sweep for unwatched channels, placeholder-manifest detection, LKG invalidation after the initial sweep, and a 7-tuple `source_rank_key` (measured > unmeasured > `success_score` > HDR > bpp > resolution > codec > fps).

## 3. Client capabilities — runtime-detected, declared at boot
- On launch, client probes capability tags via `canPlayType` AND actual-play HLS tests against `/api/probe/{video,audio}.m3u8` (canPlayType lies; play-tests are ground truth).
- Tags shipped: `hls`, `mse`, `hls_native`, `hls_mse`, `h264`, `hevc`, `vp9`, `av1`, `aac`, `mp3`, `live_video_hls`, `live_audio_only_hls`.
- Result cached in `localStorage` (UA-fingerprinted) and sent as `X-Client-Caps` header on every request.
- Server filters channels by set inclusion: `caps_required(kind) ⊆ client_caps`. Missing header = permissive (older clients unchanged).
- Implementation lives in `app/js/caps.js` and `server/src/api.rs::parse_client_caps` + `caps_required`. Today `caps_required` for TV is `["hls"]` — plumbing in place, filter not yet strict. Tightening (e.g. adding `hevc` to channels whose only sources are HEVC) is a config-level change, no new infrastructure.

## 4. "Unstable" — strictly post-`canplay` mid-stream failure
- Slow-to-start is **not** an instability signal.
- A source that reached first frame but errored during playback is unstable. Client reports `{channel, source_url, reason: "mid-playback"}` to server.
- That report puts the source into cool-off (§5).

## 5. Cool-off — exponential, reset by 5-min clean play
- Steps: 1 min → 5 min → 30 min → 6 h, escalating per consecutive failure (any cause: timeout, 4xx/5xx, abuse redirect, mid-playback error).
- **5 minutes of uninterrupted playback** on the same source resets the counter to zero. Needs a client heartbeat / "still playing" tick to the server while the channel is on.
- Cool-off state survives catalog refresh and app restart.

## 6. Startup latency budget — **5 s per URL to first frame**
- For each candidate URL the client tries, budget = 5 s to reach `canplay`.
- Timeout → client signals failure, server returns next URL.
- Today: `STALL_WATCHDOG_MS = 10000` in `player.js`. Halving the budget plus making the failure signal feed cool-off is the work.

## 7. User override — escape hatch only
- Left / right while playing cycles through the candidate list manually.
- Auto-pick is expected to be right ~99% of the time; manual cycling exists for the 1% where it isn't.

## 8. **Don't discard** — overriding principle
- Every `(source × host)` pair stays in the catalogue.
- Filters de-prioritise or hide temporarily — never permanently remove.
- The existing safety valve in `proxy.rs::build_candidates` (fall back to the unfiltered list when all filters wipe everything) is the embodiment of this rule and must stay.

---

## What's NOT changing
- Canonical merging rules (`canonical.rs` already handles RTP 1 ≡ RTP 1 HD ≡ MEO: RTP 1 etc.; RTP 1 ≠ RTP 1 Madeira).
- Catch-up plays a single source (highest-scored archive-enabled variant).
- Background opportunistic validation runs as today.

## Gaps from today's implementation

### Already done (uncommitted in worktree)
- **Client capability declaration**: `app/js/caps.js` probes, server `parse_client_caps` filters. `caps_required` is permissive today; tightening is config-level.
- **HEVC blanket filter removed** (`546af6c`) — intentional: measurement-driven ranking replaces blanket codec filtering. Combined with caps, channels whose only viable source is HEVC can be hidden per-client by adding `hevc` to their `caps_required`.

### Not yet done
- **Measured-quality record** — `score_variant` still reads names. `docs/plan-measured-quality.md` is the blueprint (new modules `measured.rs`, `probe.rs`; extend `codec.rs::classify_ts_chunk` with SPS-parsed width/height/framerate/pix_fmt/color_transfer; sweep + per-play paths; `success_score`; placeholder defence).
- **Mid-playback failure feedback** — client's `onSourceFailed` fires on media error today but the post-`canplay` distinction isn't propagated to the server as a separate signal.
- **Stepped cool-off** — `blacklist.rs` is binary (failed or not), not 1m → 5m → 30m → 6h.
- **5-min clean-play reset** — no playback heartbeat from client; nothing on the server resets the counter.
- **5 s startup budget** — `STALL_WATCHDOG_MS = 10000` in `player.js`.
- **Radio audio-quality ranking** — dormant: radio is single-source today (`direct_source` URLs from `radios.m3u`). The measured-quality plan explicitly punts on per-play radio measurement; sweep may or may not capture radio depending on container format.
