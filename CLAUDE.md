# iptv — Claude workflows

LG webOS TV app for Xtream Codes IPTV. Vanilla JS, no framework.
Lives in `/home/mgl/projects/pessoal.worktrees/iptv`.

## The TV

| | |
|---|---|
| Model | LG OLED65B46LA (2024 B4, 65") |
| webOS | 9.2.4 (codename `ombre-okapi`) |
| IP | `192.163.2.153` |
| SSH port | `9922` |
| SSH user | `prisoner` (sandboxed dev account) |
| State | Rooted via webosbrew, Homebrew Channel 0.7.2 installed |
| App ID | `com.mgl.xtream` (lives at `/media/developer/apps/usr/palm/applications/com.mgl.xtream/`) |
| Chrome DevTools port | `9998` (on TV; we tunnel it to localhost) |

The Xtream account credentials and the host list live in `app/js/config.js` (gitignored).

## SSH access

The TV's dev-mode SSH runs OpenSSH 6.1 (ancient) and signs with RSA-SHA1. Fedora's crypto policy rejects this by default. Workaround:

- Env var `OPENSSL_ENABLE_SHA1_SIGNATURES=1`
- Flags `-o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedAlgorithms=+ssh-rsa`

Use `scripts/tv-ssh` which bakes all of this in:

```bash
./scripts/tv-ssh                  # interactive shell
./scripts/tv-ssh "ls /tmp"        # one-shot command (TTY is forced — luna-send-pub needs it)
```

The wrapper uses key `~/.ssh/lgtv_dev` (unencrypted copy). One-time setup creates it from `~/.webos/ose/novacom-devices.json`:

```bash
make setup
```

## Deploy

```bash
make deploy        # ~2 s: ares-package → scp IPK → luna install → launch
```

Why not rsync directly? `prisoner` can't write `/media/developer/apps/...` (root-owned). And we can't use `ares-install` either — its bundled `ssh2` 0.8.9 chokes on the TV's SHA1 RSA signatures. So the deploy script does the install path manually:

1. `ares-package app/ -o .build/` (this DOES work — only the install transport is broken)
2. `scp` the IPK to `/media/developer/temp/`
3. `luna-send-pub` to `luna://com.webos.appInstallService/dev/install` (waits for `state:"installed"`)
4. `luna-send-pub` to `luna://com.webos.applicationManager/launch`

Other Make targets: `make launch`, `make close`, `make ssh`, `make logs`, `make clean`.

## Laptop dev / Playwright (test without touching the TV)

For UI work — layout, sort, recents, EPG, panel navigation, mini ↔ fullscreen transitions — drive the app on the laptop. No deploy needed. Much faster iteration than the TV cycle.

```bash
make serve          # → http://localhost:8000 ; runs scripts/dev-serve
```

`scripts/dev-serve` is a tiny Python HTTP server that does two dev-only things over the stock `http.server`:

1. **`Cache-Control: no-store`** on every response.
2. **Rewrites every `import './foo.js'` to `import './foo.js?v=<startup-stamp>'`** before serving the JS. ES modules are cached *by URL* in Chromium across navigations and even across Playwright sessions — without this rewrite, edited modules silently keep using the old version. The stamp is fixed per process, so all modules in one page load resolve to a consistent graph; restart the server to bust.

Once it's up, drive Chromium via the Playwright MCP (`mcp__playwright__browser_*`). Real Xtream API works from `localhost` (provider allows CORS), so all 412 channels actually load. Fedora's Chromium **does** decode `application/vnd.apple.mpegurl` natively (ffmpeg-backed), so real playback works on the laptop — `__app.player.play(ch.play_url)` paints pixels, and mini ↔ fullscreen layouts are testable end-to-end without poking state.

### Laptop keys

| TV remote | Laptop |
|---|---|
| Arrows ◁△▷▽ | Arrow keys |
| OK | Enter |
| Back | Backspace / Escape |
| Channel ▲▼ | PageUp / PageDown |
| Red | **R** or F1 |
| Green | **G** or F2 |
| Yellow | **Y** or F3 |
| Blue | **B** or F4 |
| (none) | **Home / End** — jump to top / bottom of list |

These laptop aliases live in `remote.js` alongside the real LG keycodes. The letter aliases are suppressed while an `<input>` is focused so they don't hijack search typing.

### Testing the mini ↔ fullscreen flow

Just play a channel for real — Chromium decodes HLS natively here, so no fake video / state poking is needed:

```js
// Drive the actual pipeline: app's player.play() builds the <video>, sets src, lets
// state.playing follow naturally. Then dispatch Backspace to shrink to mini, etc.
const ch = window.__app.state.channels[0];
window.__app.state.playing = { channel: ch, mode: 'live' };
document.body.classList.add('playing');
window.__app.player.play(ch.play_url);
```

For event dispatching from `browser_evaluate`, always send `keydown` *and* `keyup` — keydown alone leaves the hold-to-repeat timer running and the focus index will overshoot.

### What you can and can't verify locally

| | Local laptop | TV deploy |
|---|---|---|
| Sort order, dedup, default-order curation | ✓ | ✓ |
| Recents pinning + Red unpin | ✓ | ✓ |
| EPG fetch (`get_short_epg`, `get_simple_data_table`) | ✓ | ✓ |
| Panel navigation (list / settings / EPG) | ✓ | ✓ |
| Mini ↔ fullscreen layout transitions | ✓ | ✓ |
| Source-switch flicker, video sizing | ✓ | ✓ |
| HLS / TS playback | ✓ (Fedora Chromium has native HLS) | ✓ |
| `max-activated-media-players=1` quirks | ✗ | ✓ |

So: do UI/state work locally, deploy to the TV only when actually verifying playback or webOS-specific behaviour.

## Self-test loop (the killer workflow)

The TV's Chromium exposes Chrome DevTools Protocol on port 9998. We SSH-tunnel it and drive the app over CDP. Three scripts make it trivial:

```bash
./scripts/tv-tunnel start                    # forward localhost:9998 → TV's 9998
./scripts/tv-eval 'expression'               # eval JS in the running app, return result
./scripts/tv-key down|ok|yellow|...          # inject a remote keystroke
./scripts/tv-shot file.png                   # screenshot of the webview
```

The app exposes its state to CDP via `window.__app`:

```js
window.__app = { state, xtream, player, timings, hosts, workingHosts, failedUrls, hostFailedStreams }
```

So you can do things like:

```bash
./scripts/tv-eval 'JSON.stringify(__app.state.channels.length)'
./scripts/tv-eval 'JSON.stringify(__app.workingHosts)'
./scripts/tv-eval '__app.state.channels.filter(c => /rtp/i.test(c.name)).map(c => c.name)'
./scripts/tv-eval '__app.player.video && __app.player.video.readyState'
```

And drive the UI:

```bash
./scripts/tv-key yellow                 # open search
./scripts/tv-key down                   # focus next item
./scripts/tv-key ok                     # play / select
./scripts/tv-key back                   # exit playback / close search
```

Screenshots see the Chromium webview only — the TV's video pipeline is a hardware overlay underneath, invisible to `Page.captureScreenshot`. Use video state (`readyState`, `currentTime`, `error`) to verify playback programmatically.

If the tunnel dies (e.g., after a long pause), restart it:

```bash
./scripts/tv-tunnel stop && ./scripts/tv-tunnel start
```

If `tv-eval --list` doesn't show "Xtream", the app crashed — `make launch`.

### Useful canned snippets

```bash
# Verify a stream URL works from the TV's network
./scripts/tv-ssh "curl -s -o /tmp/x -w 'HTTP=%{http_code} bytes=%{size_download}\n' -L -m 5 'http://.../stream.ts'"

# Clear all client-side state (cache, hosts, prefs, recents, search history) and reload
./scripts/tv-eval 'localStorage.clear(); location.reload(); "reloading"'

# Find a channel by name and play it
./scripts/tv-eval 'var i = __app.state.channels.findIndex(c => /^rtp 1$/i.test(c.name)); __app.state.focusIdx = i; i'
./scripts/tv-key ok
```

### In-app event log

The app maintains a 300-entry circular buffer at `window.__app.logs`. Every probe, play, source try, source fail, and canplay is logged with a timestamp.

```bash
scripts/tv-log              # 30 most recent events
scripts/tv-log 100          # 100 most recent
scripts/tv-log fail         # only failures
scripts/tv-log try          # only "trying source N" events
scripts/tv-log probe        # only host probe results
scripts/tv-log canplay      # only successful first-frames
```

### Diagnosing "channel won't play"

The single most common cause is the TV's network path to the stream backend (`*.ip2-st31.me`) being slow. The Xtream API auth/m3u8 layer is on Cloudflare and almost always fast; the redirect target is on a different cluster that can be sluggish from the TV even when the laptop on the same Wi-Fi is fast.

Recipe to distinguish app bugs from network reality:

```bash
# 1) Same URL from laptop vs TV — compare timings
URL='http://cf.8kgaminghub.shop/live/USER/PASS/STREAMID.m3u8'
curl -s -o /tmp/x -w 'HTTP=%{http_code} time=%{time_total}\n' -L -m 10 "$URL"
./scripts/tv-ssh "curl -s -o /tmp/x -w 'HTTP=%{http_code} time=%{time_total}\n' -L -m 10 '$URL'"
```

If the TV's time is much higher than the laptop's, the network is the bottleneck — the player's per-source timeout (`SOURCE_TIMEOUT_MS` in `player.js`) needs to be longer than that.

```bash
# 2) Verbose redirect chain from the TV (find where it hangs)
./scripts/tv-ssh "curl -sv -o /dev/null -m 12 -L '$URL' 2>&1 | head -60"
```

```bash
# 3) From inside the app — does fetch (CORS) AND <video> succeed for the same URL?
scripts/tv-eval --await 'new Promise(r => {
  var url = "URL_HERE";
  var v = document.createElement("video"); v.muted = true; v.autoplay = true;
  v.style.position = "fixed"; v.style.left = "-9999px";
  document.body.appendChild(v);
  var done = false; var finish = (l) => { if (done) return; done = true; r(JSON.stringify({video: l, rs: v.readyState, err: v.error && v.error.code})); v.remove(); };
  v.addEventListener("canplay", () => finish("canplay"));
  v.addEventListener("error", () => finish("error"));
  setTimeout(() => finish("timeout"), 8000);
  v.src = url; v.load();
})'
```

If fetch succeeds but `<video>` fails repeatedly, suspect webOS's `max-activated-media-players=1` constraint — too many rapid source-switches leave the media pipeline in a bad state. Recovery: `make close && make launch`.

Account-level checks:

```bash
# Account state — max_connections (concurrent stream limit) and active_cons (currently in use)
curl -s 'http://cf.8kgaminghub.shop/player_api.php?username=USER&password=PASS' | python3 -c "import sys,json; d=json.load(sys.stdin)['user_info']; print(d)"
```

If `active_cons` is at its `max_connections` cap, every new stream attempt fails until the previous connection times out on the server (~30 s). Tearing down + opening a new source can briefly trip this.

## App architecture

```
app/
  appinfo.json            webOS manifest
  index.html              entry — just loads main.js as a module
  bg.jpg                  family photo background (1920×1080)
  icon.png  largeIcon.png 80×80 and 130×130 launcher icons
  css/app.css             single stylesheet
  js/
    main.js               entry — state, render, remote handlers, boot flow
    remote.js             LG key codes + hold-to-repeat with acceleration
    xtream.js             Xtream Codes API client
    hosts.js              parallel host probe at boot (streaming, first-alive wins)
    canonical.js          dedupe + rank variants of the same channel
    cache.js              localStorage: streams, hosts, search history, recent channels
    player.js             single <video>, multi-source sequential failover with timeout
    config.js             credentials + host list (gitignored)
```

**Boot flow** (in `main.js`):

1. Render instantly from cached streams + hosts (no spinner — see memory `feedback_iptv_app_responsivity`).
2. Streaming host probe: as soon as the FIRST host auths, kick off `allLiveStreams()`.
3. Background: all probes settle, save sorted list to localStorage for next boot.
4. Build canonical list, render again with fresh data.

**Playback flow:**

- `urlsFor(channel)` = `[{ url, name, host }, ...]` ordered by variant score (RAW > 4K > FHD > HD), then by host latency. Last-known-good URL is moved to front (tried first). URLs in the session `failedUrls` map are skipped. Hosts in `hostFailedStreams` that have ≥ `HOST_FAIL_THRESHOLD` (4) distinct stream failures are filtered out entirely.
- `player.play(urls)` tries each sequentially. `error` event with code 4 = the URL is added to `failedUrls` (and contributes to its host's failure count). Timeout (15 s) = move on but don't blacklist (could be a slow but valid stream).
- On first successful `canplay`, the URL is saved as the channel's `last-known-good` for next time.
- Left / right while playing → `switchSource(±1)` cycles through the URL list.
- Up / down while playing → `zap(±1)` debounced 250 ms (so hold-to-scroll doesn't spawn many player tear-downs).

**Canonical dedup** (`canonical.js`):

Key normalization: strip prefix (`MEO:`/`VIP:`/`VO:`/`PT:`), strip quality (`HD`/`FHD`/`UHD`/`4K`/`RAW`/`SD`), strip trailing ` TV` or ` TV 1` (so `BENFICA TV` ≡ `BENFICA TV 1`, but `SPORT TV 2` stays distinct), strip accents (NFD), lowercase, alphanum only. Min-length guard so `CM TV` keeps the `TV` and matches `CMTV`. Hardcoded alias table for known abbreviations (`btv` → `benfica`, `cnnpt` → `cnnportugal`, `panda` → `canalpanda`). Mojibake fix for `Ã§` → `ç` etc. before any other processing.

Display name keeps accents and the `&` character. When variants merge, prefer the accented + longer name.

**Remote keys** (`remote.js`):

- Standard keydown handler, but ignores `e.repeat` and manages its own accelerating timer on hold (380 ms → 45 ms over ~1.5 s) for arrows + channel up/down.
- On any remote key, if an `<input>` is focused, it gets `blur()` + `readonly` + `tabindex=-1`. This stops webOS reopening the on-screen keyboard on OK after the user has started navigating the list. Pressing Yellow re-enables typing.

## Known quirks

- **`max-activated-media-players=1`**: parallel video racing for fastest source hangs the renderer. Sequential failover only.
- **`luna-send` (root-only) vs `luna-send-pub`**: the prisoner user can only use the `-pub` variant. Most "dev/" Luna URIs are accessible. Many APIs need `-i -w <ms>` to wait for streamed responses.
- **TTY required for luna-send-pub output**: SSH without `-tt` silently buffers output. `tv-ssh` forces `-tt` and the deploy script pipes through `tr -d '\r'`.
- **CORS on stream URLs**: cross-origin manual-redirect fetches return `opaqueredirect` with the Location header stripped, so we can't pre-validate stream URLs at boot. Bad hosts are detected at play time instead.
- **Stream redirect tokens**: Xtream `.m3u8` URLs redirect to a backend with a short-lived signed URL. Each request gets a different one. The `<video>` element follows the redirect itself.
- **HLS MIME on webOS**: `application/vnd.apple.mpegurl` fails. `application/x-mpegURL` works. Plain `video.src = url` works for both `.m3u8` and `.ts`. We use direct src.
- **Cloudflare abuse-page hosts**: `cf.gocdn8k.me` auths fine but stream URLs redirect to `cloudflare-terms-of-service-abuse.com`. Each failed URL on it goes into `failedUrls`; after `HOST_FAIL_THRESHOLD` (4) distinct streams have failed there, the whole host is treated as bad and skipped.
- **EPG `has_archive` only on `get_simple_data_table`**: `get_short_epg` responses omit the field entirely. And on any given stream_id, only the catch-up-enabled source carries `has_archive=1` rows — the non-archive live mirrors of the same channel return `has_archive=0` for everything. The proxy's EPG walk now prioritises archive sources (via `EpgCandidate.priority`) so the parallel walk doesn't abort early with a non-archive response.
- **Catch-up `.ts` segment prefix**: Per-segment catch-up URLs from this provider (`<host>/hls/<hash>/<YYYY-MM-DD:HH-MM>.ts?stream_id=...&type=archive`) return a 200 with `Content-Type: video/mp2t`, but the body has a 70–150-byte opaque prefix before the MPEG-TS data starts. The TS packets are 188-byte aligned after that prefix. Lenient players cope; strict ones may need the prefix stripped before playback. The proxy currently forwards bytes as-is.
