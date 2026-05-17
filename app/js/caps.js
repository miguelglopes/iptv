// Client capability probe. Runs once on first boot, caches the result in
// localStorage, and on every subsequent boot reads from cache (so the header
// goes out instantly). The server doesn't care what the test does — it just
// checks set inclusion (channel.caps_required ⊆ client_caps).
//
// Tags currently meaningful to the server:
//   - "hls"                 — this app serves HLS only; always true for this client.
//   - "h264" + "aac"        — codec capability hints from canPlayType.
//   - "live_video_hls"      — actual playback of a known-good TV HLS stream.
//   - "live_audio_only_hls" — actual playback of a known-good radio HLS stream.
//
// To add a new capability tag:
//   1. add a probe step below that resolves true/false
//   2. add the same tag to caps_required() on the server for any channel
//      kind that needs it
// No filter-side changes needed — the check is generic.

import { PROXY_BASE_URL } from './config.js';

// Probe URLs are absolute against the proxy — the page may live on a different
// origin (file:// for the TV IPK, localhost:8000 for `make serve`) than the
// API, so a relative `/api/probe/...` resolves to the wrong host. api.js does
// the same BASE-prefix trick.
var BASE = String(PROXY_BASE_URL || '').replace(/\/$/, '');

// Bumped to v3 in Phase 6 (R2 fix): added `hevc_main10` / `dvb_safe`
// to the probe matrix. Existing v2 caches don't include those tags,
// so the freshness loop's server-side tightening would silently hide
// channels until the user manually cleared cache. Bumping the key
// drops the stale cache and forces a re-probe on the next boot —
// no operator action required.
var CACHE_KEY = 'xtream.client.caps.v3';
// Phase 6: cap-matrix-version sentinel. The server emits
// `X-Caps-Matrix-Version` on every /api/channels response; we cache the
// last-seen value and re-probe on mismatch (the freshness loop may have
// tightened server-side caps in a way that needs a re-probe to keep the
// client's local set honest). Per-cap eviction counters live under
// `xtream.caps.recent.<tag>`.
var MATRIX_VERSION_KEY = 'xtream.caps.matrix_version';
var CAP_RECENT_PREFIX = 'xtream.caps.recent.';
// Server caps probe-request budget at ~2× per_attempt (≈10 s). Allow a little
// more than that here so a slow first candidate plus its retry can complete
// before we falsely conclude "this client can't decode that content shape".
var PROBE_TIMEOUT_MS = 12000;
// Cap eviction policy: this many post-canplay failures within this window
// AND zero canplay successes → drop the cap from loadCaps(). Resets on the
// next markCapSuccess. Plan §8 lines 336-338.
var CAP_EVICTION_FAILS = 3;
var CAP_EVICTION_WINDOW_MS = 24 * 60 * 60 * 1000;

// Caps the server treats as universal (always present, never inferred from
// post-canplay failures). The eviction wiring in main.js must NOT mark these
// — a canplay failure on a "live_video_hls"-required channel says the
// channel-specific cap (h264/hevc) is the suspect, not the universal HLS
// transport. Kept in sync with the server's per-kind baseline in
// `caps_cache::caps_required`.
export var UNIVERSAL_CAPS = ['hls', 'aac', 'live_video_hls', 'live_audio_only_hls', 'mse', 'hls_native', 'hls_mse'];

// Fingerprint the cache against the User-Agent so a browser upgrade that
// gains/loses a codec invalidates the cache instead of carrying a stale
// result. Format: { ua: <userAgent>, caps: [<tag>, …] }. Old v1 entries
// (caps-array-only) are ignored — they re-probe automatically.
function uaFingerprint() {
  try { return String(navigator.userAgent || ''); } catch (e) { return ''; }
}

export function loadCaps() {
  try {
    var raw = localStorage.getItem(CACHE_KEY);
    if (!raw) return null;
    var obj = JSON.parse(raw);
    if (!obj || !Array.isArray(obj.caps)) return null;
    if (obj.ua !== uaFingerprint()) return null;
    // Strip evicted caps (Phase 6 Step 8): a tag with ≥ N post-canplay
    // failures and zero successes inside the eviction window is treated
    // as if we never probed it positively. The next `setClientCaps` call
    // ships the filtered set, so the server's per-channel filter hides
    // anything that requires the dropped cap.
    return obj.caps.filter(function (tag) { return !isEvicted(tag); });
  } catch (e) { return null; }
}

export function saveCaps(caps) {
  try {
    localStorage.setItem(CACHE_KEY, JSON.stringify({ ua: uaFingerprint(), caps: caps }));
  } catch (e) {}
}

export function clearCaps() {
  try {
    localStorage.removeItem(CACHE_KEY);
    // Best-effort cleanup of any older entry lingering from previous
    // schema versions — v1 (pre UA fingerprint) and v2 (pre Phase 6
    // hevc_main10 / dvb_safe probes).
    localStorage.removeItem('xtream.client.caps.v1');
    localStorage.removeItem('xtream.client.caps.v2');
  } catch (e) {}
}

// --- Phase 6: cap-matrix versioning + per-cap eviction ----------------------

/// Return the last-seen `X-Caps-Matrix-Version` value, or empty string when
/// unset. Used by `ensureCaps` to decide whether to re-probe.
export function loadMatrixVersion() {
  try { return localStorage.getItem(MATRIX_VERSION_KEY) || ''; }
  catch (e) { return ''; }
}

/// Store the matrix version returned by the server. Called by main.js after
/// it has read `X-Caps-Matrix-Version` from the /api/channels response.
export function saveMatrixVersion(v) {
  if (!v) return;
  try { localStorage.setItem(MATRIX_VERSION_KEY, String(v)); } catch (e) {}
}

function recentKey(tag) {
  return CAP_RECENT_PREFIX + tag;
}

function loadRecent(tag) {
  try {
    var raw = localStorage.getItem(recentKey(tag));
    if (!raw) return { fails: 0, last_fail: 0, successes: 0, last_success: 0 };
    var o = JSON.parse(raw) || {};
    return {
      fails: Number(o.fails) || 0,
      last_fail: Number(o.last_fail) || 0,
      successes: Number(o.successes) || 0,
      last_success: Number(o.last_success) || 0,
    };
  } catch (e) { return { fails: 0, last_fail: 0, successes: 0, last_success: 0 }; }
}

function saveRecent(tag, rec) {
  try { localStorage.setItem(recentKey(tag), JSON.stringify(rec)); } catch (e) {}
}

function isEvicted(tag) {
  if (UNIVERSAL_CAPS.indexOf(tag) >= 0) return false;
  var r = loadRecent(tag);
  if (r.fails < CAP_EVICTION_FAILS) return false;
  if (r.successes > 0) return false;
  return (Date.now() - r.last_fail) < CAP_EVICTION_WINDOW_MS;
}

/// Record a post-canplay failure for `tag`. Returns true iff this call
/// crossed the eviction threshold (so the caller can refresh the channel
/// list to honour the now-tighter cap set).
export function markCapFailure(tag) {
  if (UNIVERSAL_CAPS.indexOf(tag) >= 0) return false;
  var r = loadRecent(tag);
  var wasEvicted = isEvicted(tag);
  r.fails += 1;
  r.last_fail = Date.now();
  saveRecent(tag, r);
  return !wasEvicted && isEvicted(tag);
}

/// Record a successful canplay for `tag` — resets fails to 0 and lifts any
/// eviction. A single success means the codec really works; subsequent
/// failures get a fresh count.
export function markCapSuccess(tag) {
  if (UNIVERSAL_CAPS.indexOf(tag) >= 0) return;
  var r = loadRecent(tag);
  r.fails = 0;
  r.successes += 1;
  r.last_success = Date.now();
  saveRecent(tag, r);
}

// Resolve to an array of capability tags. Uses cache when present so the
// header can ship on the first request after boot. On cache miss, probes once,
// caches, and resolves.
//
// Cache guard: a probe result with *neither* play-test cap is almost always
// the symptom of a transient failure (catalog not yet ready, network blip,
// CORS hiccup) rather than a genuine "can't play anything" platform. We
// surface those caps for *this* boot so the server can still filter sanely
// (in practice it filters everything out and the user sees an empty list)
// but we don't persist them — next boot reprobes. With both play tests
// passing nothing is wasted; with only one passing we still cache (it's a
// real result).
export async function ensureCaps() {
  var cached = loadCaps();
  if (cached) return cached;
  var caps = await probe();
  var anyPlayProbePassed = caps.indexOf('live_video_hls') >= 0
    || caps.indexOf('live_audio_only_hls') >= 0;
  if (anyPlayProbePassed) saveCaps(caps);
  return caps;
}

/// Phase 6: if the server's cap-matrix version has changed since the last
/// /api/channels response, the cached cap set may no longer match what's
/// actually playable on this client. Clear the caps cache and re-probe so
/// the next request goes out with a fresh, accurate set.
///
/// `lastServerVersion` is the value from the most-recent
/// `X-Caps-Matrix-Version` header. Returns the (possibly newly probed) set.
export async function ensureCapsForMatrix(lastServerVersion) {
  var stored = loadMatrixVersion();
  if (stored && lastServerVersion && stored !== lastServerVersion) {
    clearCaps();
    saveMatrixVersion(lastServerVersion);
    return await ensureCaps();
  }
  if (lastServerVersion && !stored) saveMatrixVersion(lastServerVersion);
  return await ensureCaps();
}

// Each probe declares its tag + a check. Sync checks return bool; async return
// Promise<bool>. Adding a new probe is a one-line append here; the server
// filter is generic (set inclusion against caps_required), so no other
// changes are needed unless the new tag is actually used by some channel's
// caps_required on the server side.
var PROBES = [
  // Universal tautology — we executed JS, so we can do *some* HLS.
  { tag: 'hls',         check: function () { return true; } },

  // MediaSource Extensions — prerequisite for hls.js.
  { tag: 'mse',         check: function () { return typeof MediaSource !== 'undefined' && typeof MediaSource.isTypeSupported === 'function'; } },

  // Native HLS via canPlayType. Includes Safari, webOS, Chromium-freeworld.
  { tag: 'hls_native',  check: function () { var v = vEl(); return !!(v.canPlayType('application/vnd.apple.mpegurl') || v.canPlayType('application/x-mpegURL')); } },

  // hls.js loaded AND can run in this browser (MSE-backed).
  { tag: 'hls_mse',     check: function () { return typeof window.Hls === 'function' && window.Hls.isSupported(); } },

  // Codec canPlayType probes. "" = no; non-empty = yes (probably/maybe).
  { tag: 'h264',        check: function () { return !!vEl().canPlayType('video/mp4; codecs="avc1.42E01E"'); } },
  { tag: 'hevc',        check: function () { var v = vEl(); return !!(v.canPlayType('video/mp4; codecs="hvc1"') || v.canPlayType('video/mp4; codecs="hev1.1.6.L93.B0"')); } },
  { tag: 'vp9',         check: function () { return !!vEl().canPlayType('video/webm; codecs="vp9"'); } },
  { tag: 'av1',         check: function () { return !!vEl().canPlayType('video/mp4; codecs="av01.0.05M.08"'); } },
  { tag: 'aac',         check: function () { return !!aEl().canPlayType('audio/mp4; codecs="mp4a.40.2"'); } },
  { tag: 'mp3',         check: function () { return !!aEl().canPlayType('audio/mpeg'); } },

  // Async: actual play tests. canPlayType can lie (Chrome returns "maybe"
  // for HLS but silently fails to decode some HLS shapes in practice).
  // Each probe loads a hidden <video> against a real proxy URL and waits
  // for `loadedmetadata`. Two probes — one per content shape we actually
  // serve — so TV and radio are each verified by ground truth, not by
  // assumption.
  { tag: 'live_video_hls',      check: function () { return playProbe(BASE + '/api/probe/video.m3u8'); } },
  { tag: 'live_audio_only_hls', check: function () { return playProbe(BASE + '/api/probe/audio.m3u8'); } },

  // Phase 6: per-codec play probes. Bare tags for the two that the server's
  // caps_required uses directly (`hevc_main10`, `dvb_safe`) — these are
  // the only way the client can advertise those caps because canPlayType
  // doesn't expose them. The others (`h264_play`, `hevc_play`, `av1_play`)
  // are diagnostic supplements to the canPlayType probes; the server keeps
  // using the bare `h264`/`hevc`/`av1` tags (from canPlayType) for filter
  // matches, but Step 8's eviction loop drops the bare tag on three
  // post-canplay failures regardless.
  //
  // Each endpoint returns 404 when no channel of that codec exists; the
  // playProbe then resolves false and the tag stays off. Loaded in
  // parallel so boot cost is bounded by the slowest probe, not the sum.
  { tag: 'h264_play',         check: function () { return playProbe(BASE + '/api/probe/h264.m3u8'); } },
  { tag: 'hevc_play',         check: function () { return playProbe(BASE + '/api/probe/hevc.m3u8'); } },
  { tag: 'hevc_main10',       check: function () { return playProbe(BASE + '/api/probe/hevc_main10.m3u8'); } },
  { tag: 'av1_play',          check: function () { return playProbe(BASE + '/api/probe/av1.m3u8'); } },
  { tag: 'dvb_safe',          check: function () { return playProbe(BASE + '/api/probe/dvb_safe.m3u8'); } },
];

function vEl() { return document.createElement('video'); }
function aEl() { return document.createElement('audio'); }

// All probes run in parallel — sync ones resolve immediately, the two
// play-tests overlap so first-boot cost is ~one PROBE_TIMEOUT_MS, not the
// sum. Order of `caps` follows the PROBES array, not completion order.
async function probe() {
  var results = await Promise.all(PROBES.map(function (p) {
    return Promise.resolve()
      .then(function () { return p.check(); })
      .catch(function () { return false; });
  }));
  var caps = [];
  for (var i = 0; i < PROBES.length; i++) {
    if (results[i]) caps.push(PROBES[i].tag);
  }
  return caps;
}

// Generic play-probe. Loads `url` (server-side probe redirect to a real
// channel) in a hidden <video>. Resolves true only when `loadeddata` fires
// (readyState ≥ 2 = at least one decoded frame) AND `v.buffered.length > 0`.
// `loadedmetadata` (readyState 1) just confirms the demuxer parsed the
// manifest — a 10-bit-HEVC stream on a decoder that can't actually decode
// it would still hit metadata fine and falsely report capability. The
// stricter gate is what the file's header comment ("verified by ground
// truth, not by assumption") promises.
//
// Two backends checked:
//   1) hls.js if available — covers Chrome / Firefox / desktop
//   2) native <video src> — covers Safari / webOS
function playProbe(url) {
  return new Promise(function (resolve) {
    var v = document.createElement('video');
    v.muted = true;
    v.preload = 'auto';
    v.style.display = 'none';
    document.body.appendChild(v);

    var hls = null;
    var settled = false;
    function finish(value) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      try { if (hls) hls.destroy(); } catch (e) {}
      try { v.pause(); v.removeAttribute('src'); v.load(); v.remove(); } catch (e) {}
      resolve(value);
    }
    var timer = setTimeout(function () { finish(false); }, PROBE_TIMEOUT_MS);

    v.addEventListener('loadeddata', function () {
      // `buffered.length > 0` belt-and-braces against silent MSE
      // accept-then-drop on weird codec shapes (rare; costs nothing here).
      // If we got loadeddata without a buffered range, fall through to
      // PROBE_TIMEOUT_MS so the probe resolves false.
      if (v.buffered.length > 0) finish(true);
    });
    v.addEventListener('error', function () { finish(false); });

    var Hls = typeof window !== 'undefined' && window.Hls;
    if (Hls && Hls.isSupported()) {
      hls = new Hls({
        // Aligned with server's probe budget (2× per_attempt ≈ 10 s) so a slow
        // first candidate doesn't get aborted while the proxy is mid-failover.
        manifestLoadingTimeOut: 10000,
        fragLoadingTimeOut: 8000,
      });
      hls.on(Hls.Events.ERROR, function (_e, d) { if (d && d.fatal) finish(false); });
      hls.loadSource(url);
      hls.attachMedia(v);
    } else {
      v.src = url;
      v.load();
    }
  });
}
