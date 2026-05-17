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
- Tiebreak by host latency, then by per-channel LKG bias.
- Filters applied **before** ordering: client capability (§3), cool-off (§5).
- Implementation in tree: `MeasuredStore` (`server/src/measured.rs`) keyed `(stream_id, host)` with a 5-sample rolling buffer, per-play accumulator that piggybacks on `handle_ts_segment`'s TS classification, bootstrap sweep for unwatched channels (`server/src/probe.rs`), placeholder-manifest detection (`is_placeholder_manifest`), and an 8-element `source_rank_key` in `proxy.rs` (measured > `success_score` > HDR > bpp > resolution > codec > fps > raw kbps). The next layer of work — stepped cool-off, strict §8 enforcement, per-channel `caps_required`, user override — is planned in `docs/plan-source-reliability.md`.

## 3. Client capabilities — runtime-detected, declared at boot
- On launch, client probes capability tags via `canPlayType` AND actual-play HLS tests against `/api/probe/{video,audio}.m3u8` (canPlayType lies; play-tests are ground truth).
- Tags shipped: `hls`, `mse`, `hls_native`, `hls_mse`, `h264`, `hevc`, `vp9`, `av1`, `aac`, `mp3`, `live_video_hls`, `live_audio_only_hls`.
- Result cached in `localStorage` (UA-fingerprinted) and sent as `X-Client-Caps` header on every request.
- Server filters channels by set inclusion: `caps_required(kind) ⊆ client_caps`. Missing header = permissive (older clients unchanged).
- Implementation lives in `app/js/caps.js` and `server/src/api.rs::parse_client_caps` + `caps_required`. Today `caps_required` is per-`ChannelKind` (TV: `["hls", "h264", "aac", "live_video_hls"]`; Radio: `["hls", "aac", "live_audio_only_hls"]`). Per-channel codec-specific tightening (e.g. adding `hevc` to channels whose only sources are HEVC) is the next step; the plumbing supports it.

## 4. "Unstable" — strictly post-`canplay` mid-stream failure
- Slow-to-start is **not** an instability signal.
- A source that reached first frame but errored during playback is unstable. Client reports `{channel, source_url, reason: "mid-playback"}` to server.
- That report puts the source into cool-off (§5).

## 5. Cool-off — exponential, reset by 5-min clean play
- Steps: 1 min → 5 min → 30 min → 6 h, escalating per consecutive failure (any cause: timeout, 4xx/5xx, abuse redirect, mid-playback error).
- **5 minutes of error-free playback** on the same source resets the counter to zero. Only error events break the streak — pauses, channel switches, scrubs, and brief network blips that hls.js recovers from do not. The client sends a periodic "still playing" heartbeat to the server; the reset fires lazily when `now - last_error_at ≥ 5 min` AND a heartbeat is fresh (so a URL nobody is actively playing can't passively reset itself).
- Cool-off state persists to disk (`server/data/blacklist.json`, atomic flush like measured-quality) — survives catalog refresh and server restart.

## 6. Startup latency budget — **5 s per URL on the server**
- For each candidate URL the server attempts internally, budget = 5 s to a usable playlist response (`per_attempt_timeout_secs` in `server/src/config.rs`).
- Timeout → server falls through to the next candidate within the overall `play_budget_secs` ceiling (60 s default ≈ 12 attempts). The failed attempt feeds cool-off per §5.
- The client only ever sees one URL (the first that succeeded server-side); §6 governs the server's per-attempt timeout, not a client-side per-URL timer. Today: `per_attempt_timeout_secs = 5` in `server/src/config.rs:146` — already matches.

## 7. User override — escape hatch only
- Left / right while playing cycles through the candidate list manually.
- Auto-pick is expected to be right ~99% of the time; manual cycling exists for the 1% where it isn't.

## 8. **Don't discard** — overriding principle
- Every `(source × host)` pair stays in the catalogue.
- Filters de-prioritise or hide temporarily — never permanently remove.
- The existing safety valve in `proxy.rs::build_candidates` (fall back to the unfiltered list when all filters wipe everything) is the embodiment of this rule and must stay.

## 9. **Minimal transformation** — overriding principle
- Default for TS segment bodies is byte-for-byte passthrough. The proxy intercepts decisions and URLs, not bytes.
- A byte-level transformation (DVB-subtitle PID stripping, future re-mux, etc.) is justified only when the *requesting* client either (a) explicitly indicates it lacks the capability to decode the unmodified stream, or (b) hasn't declared its capabilities at all and we fall back to the safe-for-everyone behaviour. Clients that affirmatively claim the cap (e.g., `dvb_safe`) receive bytes as-is.
- The caps signal travels via the play URL, not just the `X-Client-Caps` header — the playback path (native `<video src>` on webOS, `hls.loadSource` on hls.js) bypasses any JS header-injection wrapper. The play URL carries `&caps=…` so `play_playlist` can bake the decision into each segment token at playlist-rewrite time; segment fetches honor it without re-reading any header.
- URL rewriting (HLS playlist + segment tokens) is exempt — necessary for auth (upstream credentials never reach the client), observability (per-segment cool-off attribution), and the `/seg/<token>` indirection that lets the proxy do (1) and the rest of these principles.
- Two clients hitting the same channel may receive transformed bytes for one and verbatim bytes for the other if their caps differ. That asymmetry is correct; the alternative is transforming for everyone "just in case", which violates this rule.

---

## What's NOT changing
- Canonical merging rules (`canonical.rs` already handles RTP 1 ≡ RTP 1 HD ≡ MEO: RTP 1 etc.; RTP 1 ≠ RTP 1 Madeira).
- Catch-up plays a single source (highest-scored archive-enabled variant).
- Background opportunistic validation runs as today.

## Gaps from today's implementation

### Already done
- **Measured-quality record** — `MeasuredStore` keyed `(stream_id, host)` with 5-sample rolling buffer (`server/src/measured.rs`); SPS extraction in `codec.rs::classify_ts_chunk` (width/height/framerate/pix_fmt/color_transfer); bootstrap sweep + per-play accumulator + placeholder defence (`server/src/probe.rs`); 8-element `source_rank_key` in `proxy.rs`.
- **Client capability declaration** — `app/js/caps.js` probes 12 tags at boot (cached per-UA in `localStorage`); server `parse_client_caps` filters channels by set inclusion.
- **HEVC blanket filter removed** (`546af6c`) — measurement-driven ranking replaces blanket codec filtering.
- **5 s per-attempt server-side budget** — `per_attempt_timeout_secs = 5` (`server/src/config.rs:146`).

### Not yet done
- **Mid-playback failure feedback** — `onSourceFailed` fires on media error/stall today, but the post-`canplay` distinction isn't propagated to the server as a separate signal; `FeedbackKind` has `Fail | Demote` but no `phase` field.
- **Stepped cool-off + persistence** — `blacklist.rs` is single-threshold + single-TTL, in-memory only. No 1 m → 5 m → 30 m → 6 h escalation, no `data/blacklist.json`.
- **Clean-play reset** — no client heartbeat, no server-side per-URL clean-play timer.
- **Strict filter-vs-rank discipline (§8)** — `is_url_failed`, `is_host_bad`, and `mark_segment_failure` exclude candidates from `build_candidates` rather than de-ranking them; `schedule_opportunistic_validation` pre-emptively blacklists candidates the user has never tried. The safety valve at `proxy.rs:475` only catches the all-empty case. Strict §8 requires moving these to rank-key penalties so candidates never disappear from the list (only `caps_required` may hide).
- **LKG as bias vs hard promotion** — `last_known_good` is a post-sort promotion today (`proxy.rs:441-466`); §2 calls it a "bias", so it should slot into the rank-key tuple (decayed by age) and the explicit post-sweep `clear_last_known_good` becomes unnecessary.
- **Per-channel `caps_required`** — today `caps_required` varies only by `ChannelKind`, so a channel whose only sources are HEVC is still listed for HEVC-incapable clients. Per-channel codec-specific tightening (derived from measured-quality data) lets the cap filter actually hide unplayable channels.
- **User override (§7)** — left/right is currently `moveHorizontal` (channel/panel/catchup navigation). No mechanism to cycle through the (source × host) candidates for the currently-playing channel. The Green button's `switchSource` is one-shot rotation, not a cycle.
- **Radio audio-quality ranking** — radio measurement is skipped today (raw AAC in master playlists, no TS classifier path). Will implement ADTS extraction (sample rate / channel count / bitrate) ahead of multi-source radio.
