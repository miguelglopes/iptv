// Thin fetch wrapper around the iptv-proxy HTTP API. The proxy owns Xtream auth,
// host probing, catalog, dedup, EPG aggregation, and source failover; the TV just
// consumes pre-cooked data.
import { PROXY_BASE_URL } from './config.js';

var BASE = String(PROXY_BASE_URL || '').replace(/\/$/, '');
var TIMEOUT_MS = 8000;

// Every fetch gets an AbortController with a hard timeout. The proxy is on-LAN so
// the timeout is generous, but a single hung TCP can otherwise stall boot indefinitely
// (e.g. flaky Wi-Fi between TV and laptop).
function http(path, opts) {
  var ctrl = new AbortController();
  var t = setTimeout(function () { ctrl.abort(); }, TIMEOUT_MS);
  opts = opts || {};
  opts.signal = ctrl.signal;
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

export function listChannels() {
  return json('/api/channels').then(function (rows) {
    return rows.map(function (c) {
      c.tv_archive = truthy(c.tv_archive);
      c.tv_archive_duration = intOr(c.tv_archive_duration, 0);
      return c;
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

// Tell the server about the current upstream. The server uses its own
// last-known-good per channel to know which URL to act on, so the client never
// sends the URL — just the channel key and the kind of feedback.
//   fail   → hard blacklist (URL won't be served again until TTL expires)
//   demote → soft demote (URL goes to the back; cycled back once fresh ones are exhausted)
export function reportFailure(key, error) {
  return post('/api/feedback/' + encodeURIComponent(key), { kind: 'fail', error: error || null }).catch(function () {});
}

export function demoteSource(key, error) {
  return post('/api/feedback/' + encodeURIComponent(key), { kind: 'demote', error: error || null }).catch(function () {});
}

export function getStatus() {
  return json('/api/status');
}

export function adminReprobe() {
  return http('/admin/reprobe', { method: 'POST' });
}

export function adminClearBlacklist() {
  return http('/admin/clear-blacklist', { method: 'POST' });
}

export function adminClearDemoted() {
  return http('/admin/clear-demoted', { method: 'POST' });
}

export function adminClearAllSources() {
  return http('/admin/clear-all', { method: 'POST' });
}
