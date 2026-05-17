// Thin fetch wrapper around the iptv-proxy HTTP API. The proxy owns Xtream auth,
// host probing, catalog, dedup, EPG aggregation, and source failover; the TV just
// consumes pre-cooked data.
import { PROXY_BASE_URL } from './config.js';
import {
  ensureCapsForMatrix, saveMatrixVersion, loadMatrixVersion,
} from './caps.js';

var BASE = String(PROXY_BASE_URL || '').replace(/\/$/, '');
var TIMEOUT_MS = 8000;

// Client capabilities — comma-joined tag list sent on every request as
// `X-Client-Caps`. Empty until main.js calls setClientCaps() from the result
// of ensureCaps() at boot. The server filters /api/channels by intersecting
// each channel's required caps against this set.
var clientCapsHeader = '';
export function setClientCaps(caps) {
  clientCapsHeader = Array.isArray(caps) ? caps.join(',') : '';
}

// Every fetch gets an AbortController with a hard timeout. The proxy is on-LAN so
// the timeout is generous, but a single hung TCP can otherwise stall boot indefinitely
// (e.g. flaky Wi-Fi between TV and laptop).
function http(path, opts) {
  var ctrl = new AbortController();
  var t = setTimeout(function () { ctrl.abort(); }, TIMEOUT_MS);
  opts = opts || {};
  opts.signal = ctrl.signal;
  // Merge X-Client-Caps into headers. Server treats absence as "permissive
  // mode" (no filtering) for back-compat with older clients that don't probe.
  if (clientCapsHeader) {
    opts.headers = Object.assign({}, opts.headers || {}, { 'X-Client-Caps': clientCapsHeader });
  }
  return fetch(BASE + path, opts).finally(function () { clearTimeout(t); });
}

function json(path) {
  return http(path).then(function (r) {
    if (!r.ok) throw new Error('HTTP ' + r.status + ' on ' + path);
    return r.json();
  });
}

function post(path, body) {
  return http(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body)
  });
}

// Xtream's tv_archive comes through as 0/1 (number or string). Coerce to a clean bool
// so consumers don't have to think about it. Missing field = no catch-up support.
function truthy(v) {
  if (v === 1 || v === true) return true;
  if (typeof v === 'string') return v === '1' || v === 'true';
  return false;
}
function intOr(v, fallback) {
  if (typeof v === 'number') return v;
  if (typeof v === 'string' && v) { var n = parseInt(v, 10); return isNaN(n) ? fallback : n; }
  return fallback;
}

// Fetch the channel list. Honours the `X-Caps-Matrix-Version` header
// (Phase 6 Step 7): when the server's matrix version differs from the
// cached one, the client clears its cap cache, re-probes, sends the fresh
// caps in `X-Client-Caps`, and re-fetches once. `allowReprobe` guards
// against an infinite loop if the second response somehow reports a
// different version again (shouldn't happen, but defence in depth).
export function listChannels(allowReprobe) {
  if (allowReprobe === undefined) allowReprobe = true;
  return http('/api/channels').then(function (r) {
    if (!r.ok) throw new Error('HTTP ' + r.status + ' on /api/channels');
    var serverVersion = '';
    try { serverVersion = r.headers.get('x-caps-matrix-version') || ''; } catch (e) {}
    var cachedVersion = loadMatrixVersion();
    if (serverVersion && cachedVersion && serverVersion !== cachedVersion && allowReprobe) {
      return ensureCapsForMatrix(serverVersion).then(function (freshCaps) {
        setClientCaps(freshCaps);
        return listChannels(false);
      });
    }
    if (serverVersion) saveMatrixVersion(serverVersion);
    return r.json().then(function (rows) {
      return rows.map(function (c) {
        c.tv_archive = truthy(c.tv_archive);
        c.tv_archive_duration = intOr(c.tv_archive_duration, 0);
        return c;
      });
    });
  });
}

export function epgFor(key) {
  return json('/api/epg/' + encodeURIComponent(key)).then(function (rows) {
    return rows.map(function (p) {
      return {
        title: p.title || '',
        description: p.description || '',
        start: p.start ? new Date(p.start) : null,
        end: p.end ? new Date(p.end) : null,
        has_archive: truthy(p.has_archive)
      };
    });
  });
}

// Tell the server about the current upstream. The play_id is the pid this
// client baked into its /play/<key>.m3u8?pid=… URL — the server uses it to
// look up the exact upstream that was chosen for this play attempt (so the
// resulting demote/blacklist targets the URL this client actually played,
// not whatever LKG happens to be set when feedback arrives).
//   fail   → demote + count toward windowed threshold; hard blacklist once it crosses
//   demote → soft demote (URL goes to the back; cycled back once fresh ones are exhausted)
export function reportFailure(key, playId, error, phase) {
  return post('/api/feedback/' + encodeURIComponent(key), {
    kind: 'fail',
    play_id: playId || null,
    error: error || null,
    phase: phase || null
  }).catch(function () {});
}

export function demoteSource(key, playId, error, phase) {
  return post('/api/feedback/' + encodeURIComponent(key), {
    kind: 'demote',
    play_id: playId || null,
    error: error || null,
    phase: phase || null
  }).catch(function () {});
}

// Clean-play heartbeat. Fires every 30 s while playback is healthy so the
// server can reset the URL's cool-off step once we've been clean long
// enough (see server's blacklist state machine). Best-effort fire-and-
// forget — a dropped heartbeat just delays the next reset attempt by one
// tick.
export function heartbeat(playId) {
  if (!playId) return Promise.resolve();
  return post('/api/heartbeat', { play_id: playId }).catch(function () {});
}

export function getStatus() {
  return json('/api/status');
}

export function adminReprobe() {
  return http('/admin/reprobe', { method: 'POST' });
}

// Build a play URL from the channel's base play_url + per-request bits.
// Single source of truth so every play path (normal, future force-play,
// catchup) appends `pid` and `caps` consistently. The server reads `caps`
// out of the query string because the playback path bypasses our XHR
// wrapper that sets `X-Client-Caps` — webOS uses `<video src>` directly
// and hls.js's `loadSource` ships without an `xhrSetup` hook.
//
// `opts.force_url` is reserved for Step 9 (user override); not consumed
// by anything yet, but accepting it here keeps the call sites stable so
// the override PR doesn't have to re-thread every callsite.
export function buildPlayUrl(baseUrl, opts) {
  var url = String(baseUrl || '');
  var pid = opts && opts.pid;
  var caps = opts && opts.caps;
  var force = opts && opts.force_url;
  var params = [];
  if (pid) params.push('pid=' + encodeURIComponent(pid));
  if (caps && caps.length) {
    var capStr = Array.isArray(caps) ? caps.join(',') : String(caps);
    if (capStr) params.push('caps=' + encodeURIComponent(capStr));
  }
  if (force) params.push('force_url=' + encodeURIComponent(force));
  if (!params.length) return url;
  return url + (url.indexOf('?') >= 0 ? '&' : '?') + params.join('&');
}

// base64-url-no-pad (`-`/`_` alphabet, no `=` padding). Server decodes
// with the matching base64 engine. Used by `forceCandidate` to encode
// the upstream URL the user picked from the candidate overlay.
export function base64UrlNoPad(s) {
  var b64 = btoa(unescape(encodeURIComponent(String(s))));
  return b64.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

// Step 9 user override: encode the chosen upstream URL and return the full
// play URL the player should hit. main.js calls player.play() with the
// returned URL — server validates against current build_candidates and
// 404s if the URL is no longer a candidate (catalog refreshed between
// the user opening the overlay and committing).
export function forceCandidate(key, baseUrl, pid, caps, upstreamUrl) {
  return buildPlayUrl(baseUrl, {
    pid: pid,
    caps: caps,
    force_url: base64UrlNoPad(upstreamUrl),
  });
}

// Fetch the ranked candidate list for a channel (Phase 7 Step 9).
// Read-only; populates the candidate-overlay UI.
export function fetchCandidates(key) {
  return json('/api/candidates/' + encodeURIComponent(key));
}
