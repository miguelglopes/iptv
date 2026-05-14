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

var RECENT_KEY_PREFIX = 'xtream.recent.channels.v1';
var RECENT_MAX = 100;
// Timestamp of the most recent play. Auto-resume on boot uses this to decide whether
// the previously-watched channel is still a relevant default — a recent on the same
// day says "resume", a recent from last month does not.
var LAST_PLAY_TS_KEY_PREFIX = 'xtream.recent.last_play_ts.v1';

// Per-mode recents: each mode has its own list under a suffixed localStorage key.
// Switching tabs swaps the recents seamlessly. The active mode itself is persisted
// separately so the next boot resumes into the right list (see loadActiveMode).
function recentKeyFor(mode) { return RECENT_KEY_PREFIX + '.' + (mode || 'tv'); }
function lastPlayTsKeyFor(mode) { return LAST_PLAY_TS_KEY_PREFIX + '.' + (mode || 'tv'); }

// In-memory cache of the parsed recents list, scoped by mode. visibleItems /
// sortByRecency call loadRecentChannels several times per render — without this,
// every call re-parsed the JSON. Invalidated whenever we write through.
var recentCache = {};

export function loadRecentChannels(mode) {
  if (recentCache[mode]) return recentCache[mode];
  try {
    var v = JSON.parse(localStorage.getItem(recentKeyFor(mode)) || '[]');
    recentCache[mode] = Array.isArray(v) ? v : [];
  } catch (e) {
    recentCache[mode] = [];
  }
  return recentCache[mode];
}

export function pushRecentChannel(channelKey, mode) {
  if (!channelKey) return;
  var r = loadRecentChannels(mode).filter(function (k) { return k !== channelKey; });
  r.unshift(channelKey);
  if (r.length > RECENT_MAX) r.length = RECENT_MAX;
  recentCache[mode] = r;
  try { localStorage.setItem(recentKeyFor(mode), JSON.stringify(r)); } catch (e) {}
  try { localStorage.setItem(lastPlayTsKeyFor(mode), String(Date.now())); } catch (e) {}
}

export function removeRecentChannel(channelKey, mode) {
  if (!channelKey) return false;
  var cur = loadRecentChannels(mode);
  var r = cur.filter(function (k) { return k !== channelKey; });
  if (r.length === cur.length) return false;
  recentCache[mode] = r;
  try { localStorage.setItem(recentKeyFor(mode), JSON.stringify(r)); } catch (e) {}
  return true;
}

export function clearRecentChannels(mode) {
  // If a mode is passed, clear only that one. Settings "Clear recent channels"
  // calls without a mode → clear both, plus the legacy unsuffixed entries from
  // before per-mode recents existed.
  if (mode) {
    recentCache[mode] = [];
    try { localStorage.removeItem(recentKeyFor(mode)); } catch (e) {}
    try { localStorage.removeItem(lastPlayTsKeyFor(mode)); } catch (e) {}
    return;
  }
  recentCache = {};
  try {
    localStorage.removeItem(recentKeyFor('tv'));
    localStorage.removeItem(recentKeyFor('radio'));
    localStorage.removeItem(lastPlayTsKeyFor('tv'));
    localStorage.removeItem(lastPlayTsKeyFor('radio'));
    // Legacy unsuffixed keys from before per-mode recents — clean up on first
    // use so they don't sit forever.
    localStorage.removeItem(RECENT_KEY_PREFIX);
    localStorage.removeItem(LAST_PLAY_TS_KEY_PREFIX);
  } catch (e) {}
}

export function loadLastPlayTimestamp(mode) {
  try {
    var raw = localStorage.getItem(lastPlayTsKeyFor(mode));
    if (!raw) return 0;
    var n = parseInt(raw, 10);
    return isFinite(n) ? n : 0;
  } catch (e) { return 0; }
}

// Last-active mode pointer. tryAutoResume() reads this so boot resumes into
// the mode the user last had open, not arbitrarily into 'tv'. Saved by the
// mode-switch handler in main.js.
var MODE_KEY = 'xtream.active.mode.v1';
export function loadActiveMode() {
  try {
    var v = localStorage.getItem(MODE_KEY);
    return (v === 'tv' || v === 'radio') ? v : 'tv';
  } catch (e) { return 'tv'; }
}
export function saveActiveMode(mode) {
  try { localStorage.setItem(MODE_KEY, mode); } catch (e) {}
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
