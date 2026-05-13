// Thin fetch wrapper around the iptv-proxy HTTP API. The proxy owns Xtream auth,
// host probing, catalog, dedup, EPG aggregation, and source failover; the TV just
// consumes pre-cooked data.
import { PROXY_BASE_URL } from './config.js';

var BASE = String(PROXY_BASE_URL || '').replace(/\/$/, '');

function json(path) {
  return fetch(BASE + path).then(function (r) {
    if (!r.ok) throw new Error('HTTP ' + r.status + ' on ' + path);
    return r.json();
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
  // [{ key, name, logo?, default_rank?, source_count, play_url, tv_archive?, tv_archive_duration? }, ...]
  return json('/api/channels').then(function (rows) {
    return rows.map(function (c) {
      c.tv_archive = truthy(c.tv_archive);
      c.tv_archive_duration = intOr(c.tv_archive_duration, 0);
      return c;
    });
  });
}

export function epgFor(key) {
  // [{ title, description, start (ISO), end (ISO), has_archive? }, ...] — server walks all sources.
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
  return fetch(BASE + '/api/feedback/' + encodeURIComponent(key), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ kind: 'fail', error: error || null })
  }).catch(function () {});
}

export function demoteSource(key, error) {
  return fetch(BASE + '/api/feedback/' + encodeURIComponent(key), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ kind: 'demote', error: error || null })
  }).catch(function () {});
}

export function getStatus() {
  return json('/api/status');
}

export function adminReprobe() {
  return fetch(BASE + '/admin/reprobe', { method: 'POST' });
}

export function adminClearBlacklist() {
  return fetch(BASE + '/admin/clear-blacklist', { method: 'POST' });
}

export function adminClearDemoted() {
  return fetch(BASE + '/admin/clear-demoted', { method: 'POST' });
}

export function adminClearAllSources() {
  return fetch(BASE + '/admin/clear-all', { method: 'POST' });
}
