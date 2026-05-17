# TV eval checklist

Run on the TV after each plan-step PR. Tick boxes inline; save a copy under
`scripts/tv-eval-runs/<date>.md` if you want a durable artefact. Future
plan-step PRs append their own sections.

The CDP eval helper (`scripts/tv-eval`, the Python script) is unchanged —
use it for the `state.*` inspections referenced below.

## After Step 1 (failure-phase signal)

- [ ] Tune to a known-good channel; pull network mid-playback → reconnects;
      check `/admin/recent-plays` for `phase=post-canplay` on the failure.
- [ ] Tune to a known-broken channel (cooled-off); first attempt times out →
      check `/admin/recent-plays` for `phase=pre-canplay` or absent.

## After Step 2+3 (cool-off + heartbeat)

- [ ] `/admin/status` shows persisted cool-off state across
      `docker compose restart` (look for a non-zero `url_states_count`
      surviving the restart; per-URL detail lives in
      `server/data/blacklist.json`).
- [ ] After 5 min of clean play, the URL's cool_off_step resets to 0 in
      `/admin/status` (Step 3 wires the heartbeat that triggers this; Step 2
      alone leaves the reset path inert).

## After Step 3 (clean-play heartbeat — additional)

- [ ] Play any channel; `docker logs iptv-proxy 2>&1 | grep heartbeat | tail`
      shows trace entries every ~30 s with the current `pid` and `url`.
- [ ] Press Back twice to stop; tail the logs for another ~35 s — no further
      `heartbeat` lines appear.
- [ ] Re-play, then pull the network briefly mid-play. After reconnect,
      heartbeats resume on the next 30 s tick (the interval keeps ticking
      across pauses; only player error/teardown clears it).

## After Step 4+5 (strict §8 rank penalty + LKG into tuple + dvb_safe)

- [ ] Inspect a play URL: `docker logs iptv-proxy | grep -m1 "play start" | head`
      and the immediately-following `play ok` line — the played URL must
      include `&caps=...` (the new query parameter; per Step 4 §9 every
      play URL constructed by buildPlayUrl carries it).
- [ ] Force three failures on the top-rank source via repeated playback
      that errors mid-stream (or via three POSTs to /api/feedback/<key>).
- [ ] Verify the channel is still listed in `/api/channels` (plan §8: no
      exclusion — the failing source is deprioritised, not removed).
- [ ] Verify the channel still plays — the next play attempt picks the
      next-ranked sibling automatically (server-side failover via the rank
      tuple, no client-side iteration).
- [ ] Wait for cool-off decay (5 min of clean heartbeats per Step 2/3, or
      manually restart the container with the persisted state cleared);
      confirm the previously-failed source rises back in candidate order.
- [ ] On a known dvb-unsafe TV channel (PCR-rides-subtitle-PID): play
      with default caps → strip applies → smooth. Verify in TV mode that
      the channel still plays (default-strip path is unchanged from
      pre-Phase-4 behaviour).
- [ ] Confirm the admin endpoints `/admin/clear-blacklist`,
      `/admin/clear-demoted`, `/admin/clear-all` now return 404
      (`curl -i -X POST http://iptv.../admin/clear-blacklist`) — they
      were removed alongside the demote bucket.

## After Step 6 (freshness loop — only if max_connections ≥ 3)

- [ ] Force-enable: set `freshness_loop_interval_secs = 60` in
      `server/config.toml` for a short run. After ~60 s,
      `curl /admin/measured-quality | jq '.[].samples[-1].at'` shows
      refreshed timestamps relative to baseline.
- [ ] Start a play; tail the server log for `freshness pass complete`
      lines — they should pause / take longer while active_plays > 0
      (the loop yields to the user via the in_flight + active_plays cap).
- [ ] Restore default config (`freshness_loop_interval_secs` unset);
      on a `max_connections ≤ 2` provider, confirm no
      `"freshness pass complete"` entries appear (auto-gated OFF).
- [ ] Set `freshness_loop_interval_secs = 0` and restart: confirm the
      loop stays off regardless of `max_connections` (incident-response
      escape hatch).

## After Step 7+8 (per-channel caps + client cap eviction)

- [ ] Find a homogeneous-HEVC channel via `/admin/measured-quality`:
      `curl /admin/measured-quality | jq '.[] | select(.aggregate.codec == "hevc")'`.
- [ ] Pre-flight 3: play it on the TV; observe `canplay`. If it fails,
      flip `TV_DECODES_HEVC_MAIN10 = false` in `proxy.rs`.
- [ ] Confirm `/api/channels` response carries `X-Caps-Matrix-Version`
      header (`curl -i /api/channels | grep -i caps`).
- [ ] Force three post-canplay HEVC failures (mid-stream errors); on the
      next list refresh, the channel disappears from the UI. Verify
      `localStorage['xtream.client.caps.v3']` no longer contains `hevc`.
- [ ] Force a measurement change (e.g. push a sample via
      `/admin/measured-quality` if writable, or wait for the freshness
      loop): confirm `X-Caps-Matrix-Version` flips and the client
      re-probes on its next `/api/channels` fetch.
- [ ] On a `dvb_unsafe` channel (PCR-rides-subtitle-PID), confirm:
      (a) `caps_required` includes `dvb_safe`, (b) a non-`dvb_safe`
      client doesn't see the channel in `/api/channels`,
      (c) a `dvb_safe` client (which we test by manually adding
      `dvb_safe` to localStorage caps + reloading) sees it and plays
      verbatim.

## After Step 9 (user override — candidate overlay)

Chord choice: **OK during fullscreen live playback opens the candidate
overlay.** OK was a no-op there before. Left/right select rows; OK again
commits a force-play; Back/Escape cancels.

- [ ] Play any channel; press OK while in fullscreen live. Candidate
      overlay appears, showing the ranked list with rank #, host,
      measured quality, and any cool-off step.
- [ ] Left/right cycles the highlighted row. The highlight follows.
- [ ] OK on a row triggers a re-play. The "forced source" overlay flashes;
      inspect `docker logs iptv-proxy | grep "force_url honoured"` for the
      server-side acknowledgement.
- [ ] Inspect the play URL the client emitted (browser devtools /
      `/admin/recent-plays`): it must include `&force_url=...&caps=...&pid=...`
      together (all three present).
- [ ] Back / Escape with the overlay open closes the overlay and returns
      to the original play (no replay).
- [ ] Manual 404 check (debug only, no in-app affordance):
      `curl -i 'http://iptv.../play/<key>.m3u8?force_url=aGVsbG8'` →
      should return 404 ("unknown force_url" — `aGVsbG8` decodes to
      "hello", which is not in any channel's candidate set).
- [ ] In mini mode (Back once from fullscreen), OK does NOT open the
      overlay — it maximises the player (existing behaviour). Overlay
      is fullscreen-live only.

## After Step 10 (radio ADTS extraction)

- [ ] `/admin/measured-quality` now contains radio entries (previously
      empty — radio was skipped at sweep time). Filter:
      `curl /admin/measured-quality | jq '.[] | select(.aggregate.codec == "aac")'`
- [ ] Radio entries report `sample_rate_hz` and `audio_channels`
      populated. Typical values: 44100 / 48000, 1 (mono) or 2 (stereo).
- [ ] For a multi-source radio channel (if any in the catalog), confirm
      `/api/candidates/<radio-key>` orders by bitrate — top entry has
      the highest `bitrate_kbps`.
- [ ] After the bootstrap sweep completes, restart the container with
      `freshness_loop_interval_secs = 60`; on the next loop tick, radio
      sample `at` timestamps advance (verifies the freshness loop also
      walks Radio now).

## Always-on regression (every ship)

Operator sanity-check after every deploy. Quick pass — should be < 5 min.

- [ ] Tune through 20 random channels (mix TV + radio); all start within
      the play budget, no "retrying…" overlay lingering longer than a
      single failover.
- [ ] EPG loads for known-EPG channels (RTP 1, SIC, TVI for TV; Antena 1,
      Antena 2 for radio).
- [ ] Catchup plays on a recent program (≤ 24 h old).
