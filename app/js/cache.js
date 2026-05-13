// Client-side persistence (localStorage). Three things live here:
//   - search history (per-device UX)
//   - recent channels (per-device UX; the server doesn't track this anymore)
//   - the last successful /api/channels payload (so we can paint instantly on boot
//     without waiting on the proxy round-trip)
//   - per-channel EPG snapshots (short TTL — proxy will refetch but we still want
//     instant paint on focus)

var SEARCH_KEY = 'xtream.search.history.v1';
var SEARCH_MAX = 5;

export function loadSearchHistory() {
  try {
    var raw = localStorage.getItem(SEARCH_KEY);
    if (!raw) return [];
    var v = JSON.parse(raw);
    return Array.isArray(v) ? v : [];
  } catch (e) { return []; }
}

export function pushSearchHistory(term) {
  if (!term || !term.trim()) return;
  var t = term.trim();
  var h = loadSearchHistory().filter(function (x) { return x !== t; });
  h.unshift(t);
  if (h.length > SEARCH_MAX) h.length = SEARCH_MAX;
  try { localStorage.setItem(SEARCH_KEY, JSON.stringify(h)); } catch (e) {}
}

export function clearSearchHistory() {
  try { localStorage.removeItem(SEARCH_KEY); } catch (e) {}
}

var RECENT_KEY = 'xtream.recent.channels.v1';
var RECENT_MAX = 100;
// Timestamp of the most recent play. Auto-resume on boot uses this to decide whether
// the previously-watched channel is still a relevant default — a recent on the same
// day says "resume", a recent from last month does not.
var LAST_PLAY_TS_KEY = 'xtream.recent.last_play_ts.v1';

export function loadRecentChannels() {
  try {
    var v = JSON.parse(localStorage.getItem(RECENT_KEY) || '[]');
    return Array.isArray(v) ? v : [];
  } catch (e) { return []; }
}

export function pushRecentChannel(channelKey) {
  if (!channelKey) return;
  var r = loadRecentChannels().filter(function (k) { return k !== channelKey; });
  r.unshift(channelKey);
  if (r.length > RECENT_MAX) r.length = RECENT_MAX;
  try { localStorage.setItem(RECENT_KEY, JSON.stringify(r)); } catch (e) {}
  try { localStorage.setItem(LAST_PLAY_TS_KEY, String(Date.now())); } catch (e) {}
}

export function removeRecentChannel(channelKey) {
  if (!channelKey) return false;
  var cur = loadRecentChannels();
  var r = cur.filter(function (k) { return k !== channelKey; });
  if (r.length === cur.length) return false;
  try { localStorage.setItem(RECENT_KEY, JSON.stringify(r)); } catch (e) {}
  return true;
}

export function clearRecentChannels() {
  try { localStorage.removeItem(RECENT_KEY); } catch (e) {}
  try { localStorage.removeItem(LAST_PLAY_TS_KEY); } catch (e) {}
}

export function loadLastPlayTimestamp() {
  try {
    var raw = localStorage.getItem(LAST_PLAY_TS_KEY);
    if (!raw) return 0;
    var n = parseInt(raw, 10);
    return isFinite(n) ? n : 0;
  } catch (e) { return 0; }
}

var CHANNELS_KEY = 'iptv.channels.v1';

export function loadChannelsCache() {
  try {
    var raw = localStorage.getItem(CHANNELS_KEY);
    if (!raw) return null;
    var obj = JSON.parse(raw);
    if (!obj || !Array.isArray(obj.channels)) return null;
    return obj.channels;
  } catch (e) { return null; }
}

export function saveChannelsCache(channels) {
  if (!Array.isArray(channels)) return;
  try { localStorage.setItem(CHANNELS_KEY, JSON.stringify({ ts: Date.now(), channels: channels })); } catch (e) {}
}

export function clearChannelsCache() {
  try { localStorage.removeItem(CHANNELS_KEY); } catch (e) {}
}

// Per-channel EPG snapshot for instant paint on focus. Server is authoritative; this
// is just a stale-but-renders cache (short TTL so we don't show yesterday's schedule).
var EPG_PREFIX = 'xtream.epg.';
var EPG_TTL_MS = 30 * 60 * 1000;

export function loadEpg(channelKey) {
  if (!channelKey) return null;
  try {
    var raw = localStorage.getItem(EPG_PREFIX + channelKey);
    if (!raw) return null;
    var obj = JSON.parse(raw);
    if (!obj || !Array.isArray(obj.programs)) return null;
    if (Date.now() - obj.ts > EPG_TTL_MS) return null;
    return obj.programs.map(function (p) {
      return {
        title: p.title,
        description: p.description,
        start: p.start ? new Date(p.start) : null,
        end: p.end ? new Date(p.end) : null,
        has_archive: !!p.has_archive
      };
    });
  } catch (e) { return null; }
}

export function saveEpg(channelKey, programs) {
  if (!channelKey || !Array.isArray(programs)) return;
  var serializable = programs.map(function (p) {
    return {
      title: p.title,
      description: p.description,
      start: p.start ? p.start.toISOString() : null,
      end: p.end ? p.end.toISOString() : null,
      has_archive: !!p.has_archive
    };
  });
  try { localStorage.setItem(EPG_PREFIX + channelKey, JSON.stringify({ ts: Date.now(), programs: serializable })); } catch (e) {}
}
