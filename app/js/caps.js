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

var CACHE_KEY = 'xtream.client.caps.v2';
// Server caps probe-request budget at ~2× per_attempt (≈10 s). Allow a little
// more than that here so a slow first candidate plus its retry can complete
// before we falsely conclude "this client can't decode that content shape".
var PROBE_TIMEOUT_MS = 12000;

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
    return obj.caps;
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
    // Best-effort cleanup of any v1 entry lingering from before the UA
    // fingerprint was added.
    localStorage.removeItem('xtream.client.caps.v1');
  } catch (e) {}
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
// channel) in a hidden <video>. If `loadedmetadata` fires inside the timeout,
// the platform decodes that content shape. Two backends checked:
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

    v.addEventListener('loadedmetadata', function () { finish(true); });
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
