import { initRemote, setRemoteHandlers } from './remote.js';
import { Player } from './player.js';
import {
  loadSearchHistory, pushSearchHistory, clearSearchHistory,
  loadRecentChannels, pushRecentChannel, removeRecentChannel, clearRecentChannels,
  loadChannelsCache, saveChannelsCache, clearChannelsCache,
  loadEpg, saveEpg
} from './cache.js';
import { listChannels, epgFor, reportFailure, demoteSource, getStatus, adminReprobe, adminClearBlacklist, adminClearDemoted, adminClearAllSources } from './api.js';

// Tag the root element so TV-specific CSS (cursor: none, anything else added later) can
// gate on the real TV vs. laptop dev. webOS UA contains "Web0S" (yes, zero, not O).
if (/Web0S/i.test(navigator.userAgent)) document.documentElement.classList.add('tv');

var $app = document.getElementById('app');
var player = new Player();

// SVG "replay" glyph. webOS Chromium's default fonts ship without U+21BB, so the
// literal ↻ character renders as a tofu square on the TV. Inline SVG dodges that
// entirely — uses currentColor so callers (pill, REPLAY badge, soon badge, catch-up
// top badge) just inherit text colour.
var REPLAY_SVG_14 = '<svg class="replay-icon" viewBox="0 0 24 24" width="14" height="14" fill="currentColor" aria-hidden="true"><path d="M12 5V1L7 6l5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6H4c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/></svg>';
var REPLAY_SVG_18 = '<svg class="replay-icon" viewBox="0 0 24 24" width="18" height="18" fill="currentColor" aria-hidden="true"><path d="M12 5V1L7 6l5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6H4c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/></svg>';

var state = {
  view: 'channels',       // 'channels' | 'playing'
  channels: [],           // [{ key, name, logo?, default_rank?, source_count, play_url }]
  focusIdx: 0,
  playing: null,          // { channel }
  mini: false,            // playing in a small corner window — list/search/settings remain interactive
  zapList: null,          // snapshot of visibleChannels() at fullscreen entry; stabilizes △▽ zap
                          // against recents reordering between presses. null = no live session.
  error: null,
  search: null,           // null = closed; string = active filter (may be '')
  panel: 'list',          // 'list' | 'settings' | 'epg' | 'player'
  settingsIdx: 0,
  epg: {
    focusKey: null,       // channel key the panel currently belongs to (drives "loading…" flash)
    byKey: {},            // EPG per channel key — { programs, fetching }
    rowIdx: -1            // index into visible programs. -1 = auto-pick the "now" row on next render.
  }
};

// Latest /api/status payload (drives the header status line). null until first poll.
var serverStatus = null;
var catalogLoading = false;

function updateBodyClass() {
  var c = document.body.classList;
  c.toggle('playing', !!state.playing && !state.mini);
  c.toggle('mini', !!state.playing && !!state.mini);
  c.toggle('panel-list', state.panel === 'list');
  c.toggle('panel-settings', state.panel === 'settings');
  c.toggle('panel-epg', state.panel === 'epg');
  c.toggle('panel-player', state.panel === 'player');
  c.toggle('catchup-mode', !!(state.playing && state.playing.mode === 'catchup'));
  updateCatchupBadge();
}

// Top-left "you're not live" reminder while in catch-up. Mirrors scene 3 of the mockup:
//   ↻ CATCH-UP   Tue 12 · 21:30 — Linha da Frente
// Content depends on whether we got here via an EPG row (state.playing.catchup.program set)
// or via a "rewind from live" trigger (only atIso set). Hidden via CSS when not in
// catch-up — JS just keeps the text current.
// Make the catch-up badge briefly visible. Same 5 s auto-hide pattern as #legend so the
// two surfaces fade together. Re-fires on any remote key (see `any` handler) so the user
// can always confirm what mode they're in by touching the remote.
var catchupBadgeTimer = null;
function showCatchupBadge() {
  if (!state.playing || state.playing.mode !== 'catchup' || state.mini) return;
  var el = document.getElementById('catchup-badge');
  if (!el) return;
  el.classList.add('visible');
  if (catchupBadgeTimer) clearTimeout(catchupBadgeTimer);
  catchupBadgeTimer = setTimeout(function () { el.classList.remove('visible'); }, 5000);
}

function updateCatchupBadge() {
  var el = document.getElementById('catchup-badge');
  if (!el) {
    el = document.createElement('div');
    el.id = 'catchup-badge';
    document.body.appendChild(el);
  }
  if (!state.playing || state.playing.mode !== 'catchup') {
    el.innerHTML = '';
    el.classList.remove('visible');
    if (catchupBadgeTimer) { clearTimeout(catchupBadgeTimer); catchupBadgeTimer = null; }
    return;
  }
  var cu = state.playing.catchup || {};
  var when = '';
  if (cu.program && cu.program.start) {
    when = cu.program.start.toLocaleString(undefined, { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' });
  } else if (cu.atIso) {
    when = new Date(cu.atIso).toLocaleString(undefined, { weekday: 'short', hour: '2-digit', minute: '2-digit' });
  }
  var title = (cu.program && cu.program.title) || '';
  el.innerHTML =
    '<span class="cu">' + REPLAY_SVG_14 + ' CATCH-UP</span>' +
    '<span class="when">' + esc(when) + '</span>' +
    (title ? '<span class="ttl">— ' + esc(title) + '</span>' : '');
  // Content update only. Visibility is driven by explicit showCatchupBadge() calls
  // (entry to catch-up + any remote key) — not by every render(), or the periodic
  // /api/status poll's render call would keep resetting the 5 s fade timer.
}

// The top-right panel is settings when idle, the mini player when minimized — settings
// has no place there while watching, and the player slot has no place there when idle.
function topRightPanel() {
  return state.mini ? 'player' : 'settings';
}

// Fast path for panel changes: only touch the body class, the focused list item, and
// the settings-item focus class. A full render() would rebuild the entire DOM (414
// list items + EPG rows) which makes panel-cycling feel sluggish on the TV.
function setPanel(newPanel) {
  if (state.panel === newPanel) return;
  state.panel = newPanel;
  updateBodyClass();
  var listEl = document.getElementById('list');
  if (listEl) {
    var el = listEl.children[state.focusIdx];
    if (el) {
      el.classList.toggle('focused', newPanel === 'list');
      el.classList.toggle('focused-dim', newPanel !== 'list');
    }
  }
  var settingsItems = document.querySelectorAll('.settings-grid .settings-item');
  for (var i = 0; i < settingsItems.length; i++) {
    settingsItems[i].classList.toggle('focused', newPanel === 'settings' && i === state.settingsIdx);
  }
  // Swap focused ↔ focused-dim on the EPG row so the gold ring follows the active panel.
  var epgRow = document.querySelector('#bottom-slot .epg-row[data-i="' + state.epg.rowIdx + '"]');
  if (epgRow) {
    epgRow.classList.toggle('focused', newPanel === 'epg');
    epgRow.classList.toggle('focused-dim', newPanel !== 'epg' && state.epg.rowIdx >= 0);
  }
  if (newPanel === 'list') scheduleEpgFetch();
}

// Same idea for moving within the settings panel — just swap two classes.
function setSettingsIdx(i) {
  if (i === state.settingsIdx) return;
  var items = document.querySelectorAll('.settings-grid .settings-item');
  if (items[state.settingsIdx]) items[state.settingsIdx].classList.remove('focused');
  state.settingsIdx = i;
  if (items[i]) items[i].classList.add('focused');
}

// Place the <video> for the current mode.
//   - fullscreen → leave on document.body, clear inline styles so the CSS #player rule
//                  (position:fixed; inset:0) covers the viewport.
//   - mini       → still on document.body, but pinned via inline style over the
//                  #top-slot's bounding rect. We don't appendChild into #top-slot
//                  because webOS Chromium pauses + reloads the media element on
//                  every re-parent, which makes mini playback stall.
function attachVideoForMode() {
  var v = document.getElementById('player');
  if (!v) return;
  if (v.parentElement !== document.body) document.body.appendChild(v);
  if (state.mini) {
    var slot = document.getElementById('top-slot');
    if (!slot) return;
    // Inset by 2 px so the slot's amber border (body.mini .top-slot) stays visible.
    var r = slot.getBoundingClientRect();
    v.style.position = 'fixed';
    v.style.top = (r.top + 2) + 'px';
    v.style.left = (r.left + 2) + 'px';
    v.style.width = (r.width - 4) + 'px';
    v.style.height = (r.height - 4) + 'px';
    v.style.zIndex = '1';
    v.style.borderRadius = '6px';
  } else {
    v.style.cssText = '';
  }
}

function notifyCleared(label) {
  setOverlay('Settings', label, 'cleared');
}

function resetBlockedSourcesAction() {
  adminClearBlacklist().then(function () {
    notifyCleared('Blocked sources');
    pollStatus();
  }).catch(function () { notifyCleared('Blocked sources (request failed)'); });
}

function resetSourcePreferencesAction() {
  adminClearDemoted().then(function () {
    notifyCleared('Source preferences');
    pollStatus();
  }).catch(function () { notifyCleared('Source preferences (request failed)'); });
}

function resetAllSourceStateAction() {
  adminClearAllSources().then(function () {
    notifyCleared('All source state');
    pollStatus();
  }).catch(function () { notifyCleared('All source state (request failed)'); });
}

function reprobeAction() {
  adminReprobe().then(function () {
    notifyCleared('Host reprobe requested');
    pollStatus();
  }).catch(function () { notifyCleared('Reprobe (request failed)'); });
}

function clearSearchHistoryAction() {
  clearSearchHistory();
  notifyCleared('Search history');
  if (state.search != null) render();
}

function clearRecentChannelsAction() {
  clearRecentChannels();
  notifyCleared('Recent channels');
  render();
}

function clearChannelsCacheAction() {
  clearChannelsCache();
  notifyCleared('Cached channels (next boot refetches)');
}

function clearAllAction() {
  try { localStorage.clear(); } catch (e) {}
  location.reload();
}

var SETTINGS = [
  { label: 'Reset blocked sources',    action: resetBlockedSourcesAction },
  { label: 'Reset source preferences', action: resetSourcePreferencesAction },
  { label: 'Reset all source state',   action: resetAllSourceStateAction },
  { label: 'Reprobe hosts',            action: reprobeAction },
  { label: 'Clear search history',     action: clearSearchHistoryAction },
  { label: 'Clear recent channels',    action: clearRecentChannelsAction },
  { label: 'Clear cached channels',    action: clearChannelsCacheAction },
  { label: 'Clear all (reload)',       action: clearAllAction }
];

var timings = { boot: performance.now() };
function mark(name) {
  timings[name] = performance.now();
  if (window.__app) window.__app.timings = timings;
}

var LOG_MAX = 300;
var logBuffer = [];
function logEvent(category, message, data) {
  var entry = { t: (new Date()).toISOString().slice(11, 23), cat: category, msg: String(message || '') };
  if (data !== undefined) entry.data = data;
  logBuffer.unshift(entry);
  if (logBuffer.length > LOG_MAX) logBuffer.length = LOG_MAX;
  if (window.__app) window.__app.logs = logBuffer;
}

window.__app = { state: state, player: player, timings: timings, status: null, logs: logBuffer };

function esc(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
    return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
  });
}

function fmtTime(d) {
  if (!d) return '?';
  var h = d.getHours(); if (h < 10) h = '0' + h;
  var m = d.getMinutes(); if (m < 10) m = '0' + m;
  return h + ':' + m;
}

function dayKey(d) { return d.getFullYear() + '-' + d.getMonth() + '-' + d.getDate(); }
function dayLabel(d) {
  var today = new Date(); today.setHours(0, 0, 0, 0);
  var day = new Date(d.getFullYear(), d.getMonth(), d.getDate());
  var days = Math.round((day - today) / 86400000);
  if (days === 0) return 'Today';
  if (days === 1) return 'Tomorrow';
  if (days === -1) return 'Yesterday';
  return d.toLocaleDateString(undefined, { weekday: 'long', day: 'numeric', month: 'short' });
}

function focusedChannel() {
  var items = visibleItems();
  var it = items[state.focusIdx];
  return it && it.kind === 'channel' ? it.channel : null;
}

// Provider's archive encoder lag: programs that ended within this window on a
// catch-up channel often have has_archive=0 because the upstream hasn't finished
// encoding them yet. Empirically ~2.7h on RTP 1.
var PROVIDER_LAG_MS = 3 * 3600 * 1000;

// Programs currently visible in the EPG panel for `ch`. Returns null if no data yet.
// The window is 1 h on regular channels (just anchor on "right now") and
// tv_archive_duration days on catch-up channels (so users can scroll into history).
// Cache the filtered "visible" list per channel. The cutoff drifts at most a few
// seconds between presses, so we keep the cache for a minute and skip the O(n) filter
// on hot-path navigation.
var visibleEpgCache = { key: null, computedAt: 0, list: null };
function visibleEpgPrograms(ch) {
  if (!ch) return null;
  var entry = state.epg.byKey[ch.key];
  var programs = entry && entry.programs;
  if (!programs || !programs.length) return null;
  var now = Date.now();
  if (visibleEpgCache.key === ch.key && (now - visibleEpgCache.computedAt) < 60000) {
    return visibleEpgCache.list;
  }
  var pastWindowMs = ch.tv_archive
    ? Math.max(ch.tv_archive_duration, 1) * 86400000
    : 3600000;
  var cutoff = now - pastWindowMs;
  var visible = programs.filter(function (p) { return p.end && p.end.getTime() > cutoff; });
  var result = visible.length ? visible : null;
  visibleEpgCache = { key: ch.key, computedAt: now, list: result };
  return result;
}

// Index of the program that's airing right now within `visible`. -1 if none (e.g.,
// rare EPG gap or transient mid-render moment).
function nowEpgIndex(visible) {
  if (!visible) return -1;
  var now = Date.now();
  for (var i = 0; i < visible.length; i++) {
    var p = visible[i];
    if (p.start && p.end && p.start.getTime() <= now && now < p.end.getTime()) return i;
  }
  return -1;
}

function epgPanelHtml() {
  var ch = focusedChannel();
  if (!ch) return '<div class="epg-empty">no channel focused</div>';

  // Channel header — channel name + optional catch-up pill. Always rendered so the
  // panel is self-describing.
  var head = '<div class="epg-channel">' + esc(ch.name);
  if (ch.tv_archive && ch.tv_archive_duration > 0) {
    head += '<span class="epg-catchup-pill">' + REPLAY_SVG_14 + 'CATCH-UP · ' + ch.tv_archive_duration + 'd</span>';
  }
  head += '</div>';

  var entry = state.epg.byKey[ch.key];
  if ((!entry || !entry.programs) && entry && entry.fetching) return head + '<div class="epg-empty">loading…</div>';
  if (!entry || !entry.programs || !entry.programs.length) return head + '<div class="epg-empty">no schedule info</div>';

  var visible = visibleEpgPrograms(ch);
  if (!visible) return head + '<div class="epg-empty">no upcoming programs</div>';

  // Default the row focus to the "now" row on first render of this channel, or after
  // the user explicitly asked for a reset (rowIdx === -1). Otherwise clamp the existing
  // index in case `visible` shifted as time passed.
  if (state.epg.rowIdx < 0) {
    var nowIdx = nowEpgIndex(visible);
    state.epg.rowIdx = nowIdx >= 0 ? nowIdx : 0;
  }
  if (state.epg.rowIdx >= visible.length) state.epg.rowIdx = visible.length - 1;

  var now = Date.now();
  var epgActive = state.panel === 'epg';

  var rows = '';
  var lastDay = null;
  for (var i = 0; i < visible.length; i++) {
    var p = visible[i];
    if (p.start) {
      var day = dayKey(p.start);
      if (day !== lastDay) {
        rows += '<div class="epg-day">' + esc(dayLabel(p.start)) + '</div>';
        lastDay = day;
      }
    }
    var isNow = p.start && p.end && p.start.getTime() <= now && now < p.end.getTime();
    var isPast = p.end && p.end.getTime() <= now;

    var rowCls = 'epg-row';
    var badge = '';
    if (isNow) {
      rowCls += ' now';
      badge = '<div class="epg-now-badge">LIVE</div>';
    } else if (isPast) {
      if (ch.tv_archive && p.has_archive) {
        rowCls += ' past-replay';
        badge = '<div class="epg-replay-badge">' + REPLAY_SVG_18 + '<span class="lbl">REPLAY</span></div>';
      } else if (ch.tv_archive && (now - p.end.getTime()) < PROVIDER_LAG_MS) {
        // Channel supports catch-up + just-ended + not yet archived → "soon" state.
        rowCls += ' past-pending';
        badge = '<div class="epg-pending-badge">' + REPLAY_SVG_14 + 'soon</div>';
      } else {
        // Either channel doesn't support catch-up at all, or program is past
        // the archive window — aired and gone.
        rowCls += ' past-gone';
      }
    }
    if (i === state.epg.rowIdx) rowCls += epgActive ? ' focused' : ' focused-dim';

    rows += '<div class="' + rowCls + '" data-i="' + i + '">' +
              badge +
              '<div class="epg-time">' + esc(fmtTime(p.start)) + ' – ' + esc(fmtTime(p.end)) + '</div>' +
              '<div class="epg-prog">' + esc(p.title || '—') + '</div>' +
            '</div>';
  }
  return head + rows;
}

var epgFetchTimer = null;
function scheduleEpgFetch() {
  // Cheap synchronous swap: show "loading…" the moment focus changes so the user
  // never sees the previous channel's schedule. The full EPG render (which can be
  // ~200 DOM nodes for a multi-day schedule) is deferred to after the debounce,
  // so rapid scrolling stays snappy.
  var ch = focusedChannel();
  if (ch && state.epg.focusKey !== ch.key) {
    state.epg.focusKey = ch.key;
    state.epg.rowIdx = -1;  // auto-pick "now" on next render of this channel
    var panel = document.getElementById('bottom-slot');
    if (panel) panel.innerHTML = '<div class="epg-empty">loading…</div>';
  }
  if (epgFetchTimer) clearTimeout(epgFetchTimer);
  epgFetchTimer = setTimeout(fetchEpgForFocused, 80);
}

function fetchEpgForFocused() {
  var ch = focusedChannel();
  if (!ch) return;

  // Cache hit — render and skip the network.
  var cached = loadEpg(ch.key);
  if (cached && cached.length) {
    state.epg.byKey[ch.key] = { programs: cached, fetching: false };
    renderEpgPanel();
    return;
  }

  // Already fetched (possibly returned []) — re-render whatever we have.
  var entry = state.epg.byKey[ch.key];
  if (entry && entry.programs != null) {
    renderEpgPanel();
    return;
  }

  state.epg.byKey[ch.key] = { programs: null, fetching: true };
  // Server walks all sources in parallel and returns the best EPG it could find,
  // already deduped — no client-side fallback walk needed.
  epgFor(ch.key).then(function (programs) {
    if (state.epg.focusKey !== ch.key) return;
    state.epg.byKey[ch.key] = { programs: programs, fetching: false };
    saveEpg(ch.key, programs);
    renderEpgPanel();
  }).catch(function () {
    if (state.epg.focusKey !== ch.key) return;
    state.epg.byKey[ch.key] = { programs: [], fetching: false };
    saveEpg(ch.key, []);
    renderEpgPanel();
  });
}

function renderEpgPanel() {
  var panel = document.getElementById('bottom-slot');
  if (!panel) return;
  panel.innerHTML = epgPanelHtml();
  // Center the focused row (which defaults to "now" on a fresh channel) so the user
  // sees what they have selected without scrolling.
  var focused = panel.querySelector('.epg-row.focused, .epg-row.focused-dim');
  var target = focused || panel.querySelector('.epg-row.now');
  if (target && target.scrollIntoView) target.scrollIntoView({ block: 'center' });
}

function listHtml(items) {
  var html = '';
  var listActive = state.panel === 'list';
  for (var i = 0; i < items.length; i++) {
    var focus = (listActive && i === state.focusIdx) ? ' focused' : (i === state.focusIdx ? ' focused-dim' : '');
    var it = items[i];
    if (it.kind === 'recent') {
      html += '<div class="list-item recent' + focus + '" data-i="' + i + '"><span class="ret">↩</span> ' + esc(it.text) + '</div>';
    } else {
      var played = it.played ? ' played' : '';
      html += '<div class="list-item' + played + focus + '" data-i="' + i + '">' + esc(it.channel.name) + '</div>';
    }
  }
  return html;
}

// Server returns channels already sorted (default_rank then alpha). The client adds
// the per-device "recently played" preference on top: pinned channels float to the
// top in their stored order.
function sortByRecency(channels) {
  var recent = loadRecentChannels();
  if (!recent.length) return channels.slice();
  var recentRank = {};
  for (var i = 0; i < recent.length; i++) recentRank[recent[i]] = i;
  var keyed = channels.map(function (c, idx) {
    var r = recentRank[c.key];
    return { c: c, r: r !== undefined ? r : Infinity, i: idx };
  });
  keyed.sort(function (a, b) {
    if (a.r !== b.r) return a.r - b.r;
    return a.i - b.i;
  });
  return keyed.map(function (k) { return k.c; });
}

function visibleChannels() {
  var ranked = sortByRecency(state.channels);
  if (!state.search) return ranked;
  var s = state.search.toLowerCase();
  return ranked.filter(function (c) { return c.name.toLowerCase().indexOf(s) >= 0; });
}

// What the list renders: recent searches (when search is open & empty), else channels.
function visibleItems() {
  if (state.search === '') {
    var h = loadSearchHistory();
    if (h.length) return h.map(function (t) { return { kind: 'recent', text: t }; });
  }
  var recent = loadRecentChannels();
  var played = {};
  for (var i = 0; i < recent.length; i++) played[recent[i]] = true;
  return visibleChannels().map(function (c) {
    return { kind: 'channel', channel: c, played: !!played[c.key] };
  });
}

function hostStatusLabel() {
  if (!serverStatus) {
    return catalogLoading
      ? '<span class="spin">●</span> contacting proxy…'
      : '<span class="spin">●</span> waiting for proxy';
  }
  var h = serverStatus.hosts || {};
  var total = h.total || 0;
  var alive = h.alive || 0;
  var disabled = h.blacklisted || 0;
  if (alive === 0 && total > 0) {
    return '<span class="spin">●</span> probing ' + total + ' hosts';
  }
  return '<span class="ok">✓</span> ' + alive + ' hosts' + (disabled ? ' (' + disabled + ' disabled)' : '');
}

function render() {
  if (state.error) {
    $app.innerHTML = '<header><h1>Error</h1></header><pre class="error">' + esc(state.error) + '</pre>';
    return;
  }
  var visible = visibleItems();
  var searchBar = '';
  if (state.search != null) {
    searchBar = '<input id="search" class="search" placeholder="search…" value="' + esc(state.search) + '" autocomplete="off">';
  }
  // Top-slot contents: settings (2-column grid) when idle/fullscreen; empty when in mini
  // (we re-parent the <video> element here in attachVideoForMode after render).
  var topSlotHtml;
  if (state.mini) {
    topSlotHtml = '';
  } else {
    var settingsHtml = '';
    for (var si = 0; si < SETTINGS.length; si++) {
      var s = SETTINGS[si];
      var focused = state.panel === 'settings' && si === state.settingsIdx ? ' focused' : '';
      var div = s.divider ? ' divider' : '';
      settingsHtml += '<div class="settings-item' + focused + div + '" data-i="' + si + '">' + esc(s.label) + '</div>';
    }
    topSlotHtml =
      '<div class="settings-title">Settings</div>' +
      '<div class="settings-grid">' + settingsHtml + '</div>';
  }
  // Keys legend lives in the header now — frees the right column for settings + guide.
  var keymapInline =
    '<div class="keymap-inline">' +
      '<span><span class="k k-y">●</span> Y Search</span>' +
      '<span><span class="k k-r">●</span> R Unpin</span>' +
      '<span>OK Play</span>' +
      '<span>◁▷ Panel</span>' +
      '<span>△▽ Channel</span>' +
    '</div>';
  $app.innerHTML =
    '<section class="left-col">' +
      '<header>' +
        '<div class="header-top">' +
          '<h1>Channels</h1>' +
          '<div class="hint">' + visible.length + (state.search ? ' / ' + state.channels.length : '') + ' · ' + hostStatusLabel() + '</div>' +
        '</div>' +
        keymapInline +
      '</header>' +
      searchBar +
      '<section id="list" class="list">' + listHtml(visible) + '</section>' +
    '</section>' +
    '<aside class="right-col">' +
      '<div id="top-slot" class="top-slot">' + topSlotHtml + '</div>' +
      '<div id="bottom-slot" class="bottom-slot">' + epgPanelHtml() + '</div>' +
    '</aside>';
  attachVideoForMode();
  updateBodyClass();
  scrollToFocus();
  if (state.panel === 'list') scheduleEpgFetch();
  if (state.search != null) {
    var inp = document.getElementById('search');
    if (inp) {
      if (focusSearchOnNextRender) {
        inp.focus();
        try { inp.setSelectionRange(inp.value.length, inp.value.length); } catch (e) {}
        focusSearchOnNextRender = false;
      }
      inp.oninput = function () {
        state.search = inp.value;
        state.focusIdx = 0;
        var list = document.getElementById('list');
        if (list) list.innerHTML = listHtml(visibleItems());
        scheduleEpgFetch();
      };
    }
  }
}

var focusSearchOnNextRender = false;

function scrollToFocus() {
  var el = $app.querySelector('.list-item.focused');
  if (el && el.scrollIntoView) el.scrollIntoView({ block: 'center' });
}

// Hold-to-fast-scroll: when arrow events come in faster than 80 ms apart, ramp up the
// step size so a sustained hold flies through the list (or the EPG) instead of crawling
// one item at a time. Resets to 1 after any pause.
var lastMoveTs = 0;
var moveBoost = 1;
function focusStep(delta, max) {
  var now = performance.now();
  if (now - lastMoveTs < 80) moveBoost = Math.min(moveBoost + 0.18, max);
  else moveBoost = 1;
  lastMoveTs = now;
  var step = Math.max(1, Math.round(Math.abs(delta) * moveBoost));
  return delta < 0 ? -step : step;
}

function moveFocus(delta) {
  if (state.playing && !state.mini) return zap(delta);
  if (state.panel === 'settings') {
    var next = state.settingsIdx + delta;
    if (next >= SETTINGS.length) {
      // Past the last item → drop into the EPG panel below.
      setPanel('epg');
      return;
    }
    if (next < 0) next = SETTINGS.length - 1;
    setSettingsIdx(next);
    return;
  }
  if (state.panel === 'player') {
    // Mini-only: down drops focus into the EPG below the player.
    if (delta > 0) setPanel('epg');
    return;
  }
  if (state.panel === 'epg') {
    var ch = focusedChannel();
    var visible = visibleEpgPrograms(ch);
    if (!visible || !visible.length) {
      // No EPG data yet (loading / empty). Going up still escapes to the top-right slot.
      if (delta < 0) {
        var topEmpty = topRightPanel();
        if (topEmpty === 'settings') state.settingsIdx = SETTINGS.length - 1;
        setPanel(topEmpty);
      }
      return;
    }
    var prevIdx = state.epg.rowIdx;
    if (prevIdx < 0) prevIdx = 0;
    // Higher acceleration cap than the channel list (8) because the EPG can have
    // 200+ rows and a sustained hold should fly through.
    var step = focusStep(delta, 12);
    var nextIdx = prevIdx + step;
    if (nextIdx < 0) {
      // Past the top → escape up to the top-right slot.
      var top = topRightPanel();
      if (top === 'settings') state.settingsIdx = SETTINGS.length - 1;
      setPanel(top);
      return;
    }
    if (nextIdx >= visible.length) nextIdx = visible.length - 1;
    state.epg.rowIdx = nextIdx;
    // Only retag the two affected rows. The EPG can be ~90 nodes; iterating every
    // arrow press would re-render the whole DOM tree and feel sluggish on the TV.
    var panel = document.getElementById('bottom-slot');
    if (panel) {
      var prevEl = panel.querySelector('.epg-row[data-i="' + prevIdx + '"]');
      var nextEl = panel.querySelector('.epg-row[data-i="' + nextIdx + '"]');
      if (prevEl) prevEl.classList.remove('focused');
      if (nextEl) {
        nextEl.classList.add('focused');
        nextEl.scrollIntoView({ block: 'nearest' });
      }
    }
    return;
  }
  var list = visibleItems();
  if (!list.length) return;
  var step = focusStep(delta, 8);
  // Clamp instead of wrapping — wrapping a 400-item list disorients the user.
  var next = state.focusIdx + step;
  if (next < 0) next = 0;
  if (next >= list.length) next = list.length - 1;
  var prevIdx = state.focusIdx;
  state.focusIdx = next;
  // Only touch the two list items that actually changed — iterating all ~400 every
  // arrow press is what was making fast scrolling feel sluggish on the TV.
  var listEl = document.getElementById('list');
  if (listEl) {
    var prevEl = listEl.children[prevIdx];
    var nextEl = listEl.children[next];
    if (prevEl && prevEl !== nextEl) prevEl.classList.remove('focused');
    if (nextEl) {
      nextEl.classList.add('focused');
      // block:'nearest' is a no-op while the item is fully visible — we only pay
      // the scroll cost when the focus is about to leave the viewport.
      nextEl.scrollIntoView({ block: 'nearest' });
    }
  }
  scheduleEpgFetch();
}

// Home/End jumps for fast traversal. Only affects the list panel.
function jumpToEdge(end) {
  if (state.playing && !state.mini) return;
  if (state.panel !== 'list') return;
  var list = visibleItems();
  if (!list.length) return;
  state.focusIdx = end ? list.length - 1 : 0;
  render();
  scheduleEpgFetch();
}

// Arrow left/right: timeline navigation. Consistent meaning regardless of playback mode:
//   - catchup → native HTML5 seek ±30 s on the VOD chunk, no reload
//   - live    → ◁ enters catch-up at "now − 30 s" (or toast if no catch-up), ▷ toast
//                "already at live"
// Source-switching moved off ◁▷ to a dedicated Green key — see `green:` handler.
// Outside playback, ◁▷ are panel switchers (list ↔ right-column).
function moveHorizontal(delta) {
  if (state.playing && !state.mini) {
    if (state.playing.mode === 'catchup') return seekCatchup(delta * 30);
    // Live playback.
    if (delta > 0) {
      setOverlay(state.playing.channel.name, '', 'already at live');
      return;
    }
    var ch = state.playing.channel;
    if (!ch.tv_archive) {
      setOverlay(ch.name, '', 'no catch-up on this channel');
      return;
    }
    enterCatchupAtNow(-30);
    return;
  }
  // Settings is a 2-column grid: left/right hop columns within the same row before
  // falling off the left edge into the list. Even index = left col, odd = right col.
  if (state.panel === 'settings') {
    if (delta > 0) {
      if (state.settingsIdx % 2 === 0 && state.settingsIdx + 1 < SETTINGS.length) setSettingsIdx(state.settingsIdx + 1);
    } else if (state.settingsIdx % 2 === 1) {
      setSettingsIdx(state.settingsIdx - 1);
    } else {
      setPanel('list');
    }
    return;
  }
  // Linear chain: list ↔ player ↔ epg (mini) or list ↔ epg (idle).
  if (delta > 0) {
    if (state.panel === 'list') setPanel(state.mini ? 'player' : 'epg');
    else if (state.panel === 'player') setPanel('epg');
  } else {
    if (state.panel === 'epg') setPanel(state.mini ? 'player' : 'list');
    else if (state.panel === 'player') setPanel('list');
  }
}

function pageJump(delta) { moveFocus(delta * 10); }

// Snapshot visibleChannels() so △▽ zap during this fullscreen session navigates a stable
// list. Each zap's pushRecentChannel call would otherwise pin the new channel to the top
// of recents and shift visibleChannels() between presses, breaking up→down→up = up.
function captureZapList() {
  state.zapList = visibleChannels();
}

function play(channel) {
  if (state.search) pushSearchHistory(state.search);
  captureZapList();
  pushRecentChannel(channel.key);
  state.playing = { channel: channel, mode: 'live' };
  state.mini = false;
  updateBodyClass();
  setOverlay(channel.name, '', 'connecting…', true);
  mark('playStart');
  logEvent('play', channel.name, { url: channel.play_url });
  player.play(channel.play_url);
}

// Build the catch-up URL for a channel. Server contract:
//   GET /play/<key>.m3u8?at=<rfc3339-utc>&duration=<minutes>
//     → VOD playlist starting at `at`, covering `duration` minutes.
// `at` and `from` are mutually exclusive on the server; we always use `at` because the
// client always has the EPG row's start time (UTC ISO via Date.toISOString()).
function catchupUrl(channel, atIso, durationMin) {
  var sep = channel.play_url.indexOf('?') >= 0 ? '&' : '?';
  return channel.play_url + sep +
    'at=' + encodeURIComponent(atIso) +
    '&duration=' + Math.max(1, Math.round(durationMin));
}

// Catch-up playback for a specific past program. State transitions to mode='catchup'
// so the rest of the app (key handlers, OSD, source-failure path) knows we're inside
// a VOD chunk rather than the live sliding window.
function playCatchup(channel, program) {
  if (!program || !program.start || !program.end) return;
  if (!channel.tv_archive) {
    setOverlay(channel.name, program.title || '', 'catch-up not supported on this channel');
    return;
  }
  captureZapList();
  pushRecentChannel(channel.key);
  var atIso = program.start.toISOString();
  // Request the full program length plus a 5-minute buffer so seeking near the boundaries
  // doesn't immediately need a chunk reload.
  var durationMin = Math.ceil((program.end.getTime() - program.start.getTime()) / 60000) + 5;
  var url = catchupUrl(channel, atIso, durationMin);
  state.playing = {
    channel: channel,
    mode: 'catchup',
    catchup: { program: program, atIso: atIso, durationMin: durationMin }
  };
  state.mini = false;
  updateBodyClass();
  showCatchupBadge();
  setOverlay(channel.name, program.title || '', 'CATCH-UP · loading…', true);
  mark('catchupStart');
  logEvent('catchup', channel.name + ' / ' + (program.title || ''), { url: url, atIso: atIso, durationMin: durationMin });
  player.play(url);
}

var zapTimer = null;
function zap(delta) {
  var list = state.zapList || visibleChannels();
  if (!list.length || !state.playing) return;
  var curIdx = -1;
  for (var i = 0; i < list.length; i++) {
    if (list[i].key === state.playing.channel.key) { curIdx = i; break; }
  }
  if (curIdx < 0) curIdx = state.focusIdx;
  state.focusIdx = (curIdx + delta + list.length) % list.length;
  var ch = list[state.focusIdx];
  // Zapping always lands on live for the new channel — even if we were watching the
  // previous channel's catch-up. Setting mode explicitly so updateBodyClass clears the
  // CATCH-UP badge and the rest of the app sees a coherent state.
  state.playing = { channel: ch, mode: 'live' };
  updateBodyClass();
  setOverlay(ch.name, '', '…', true);
  if (zapTimer) clearTimeout(zapTimer);
  zapTimer = setTimeout(function () {
    zapTimer = null;
    if (state.playing) {
      pushRecentChannel(state.playing.channel.key);
      player.play(state.playing.channel.play_url);
    }
  }, 250);
}

// Left/right while playing — demote the current upstream so the proxy picks a
// different one on the next playlist hit. Debounced 250 ms so holding the key
// doesn't tear down and recreate the <video> element repeatedly (webOS allows
// only max-activated-media-players=1, and rapid churn leaves it in a bad state).
// Demote (not fail) so an exhausted candidate pool cycles back to it instead of
// returning 503.
// Native seek within the current catch-up VOD chunk. Updates video.currentTime directly
// — no re-fetch, no re-load. If the user seeks past the chunk end (beyond `duration`)
// the upstream playlist runs out and the video naturally pauses; that case is rare on
// a 60-min default chunk and we'll surface it as a "load more" affordance later.
function seekCatchup(seconds) {
  if (!player.video) return;
  var v = player.video;
  var target = (v.currentTime || 0) + seconds;
  if (target < 0) target = 0;
  if (v.duration && isFinite(v.duration) && target > v.duration) target = v.duration;
  v.currentTime = target;
  logEvent('seek', String(seconds), { ct: v.currentTime, dur: v.duration });
  // Refresh the OSD time/scrubber if we have one (Phase 3d wires this up).
  if (state.playing && state.playing.mode === 'catchup') {
    var ch = state.playing.channel;
    setOverlay(ch.name, '', 'CATCH-UP · ' + Math.round(v.currentTime) + ' / ' + (isFinite(v.duration) ? Math.round(v.duration) : '?') + 's', true);
  }
}

var switchTimer = null;
function switchSource() {
  if (!state.playing) return;
  setOverlay(state.playing.channel.name, '', 'switching source…', true);
  logEvent('switch', state.playing.channel.name);
  if (switchTimer) clearTimeout(switchTimer);
  switchTimer = setTimeout(function () {
    switchTimer = null;
    if (!state.playing) return;
    // Await the POST so the server has demoted the LKG before the playlist refetch —
    // otherwise the refresh can race the update and the server may serve the same URL.
    demoteSource(state.playing.channel.key, 'user-requested').then(function () {
      player.refresh();
    });
  }, 250);
}

function activate() {
  if (state.playing && !state.mini) return;
  if (state.panel === 'settings') {
    var setting = SETTINGS[state.settingsIdx];
    if (setting && setting.action) setting.action();
    return;
  }
  if (state.panel === 'player') {
    // Highlighted mini player → maximize back to fullscreen.
    if (!state.playing) return;
    captureZapList();
    state.mini = false;
    state.panel = 'list';
    attachVideoForMode();
    updateBodyClass();
    setOverlay(state.playing.channel.name, '', 'fullscreen');
    return;
  }
  if (state.panel === 'epg') {
    var ch = focusedChannel();
    var visible = visibleEpgPrograms(ch);
    if (!visible || !visible.length) return;
    var p = visible[state.epg.rowIdx];
    if (!p) return;
    var now = Date.now();
    var isNow = p.start && p.end && p.start.getTime() <= now && now < p.end.getTime();
    var isPast = p.end && p.end.getTime() <= now;
    if (isNow) {
      // OK on the LIVE row → play the channel live, same as activating it from the list.
      play(ch);
      return;
    }
    if (isPast && ch.tv_archive && p.has_archive) {
      // Phase 3 will swap in real catch-up playback. For now: stub that says we got here.
      playCatchup(ch, p);
      return;
    }
    if (isPast && ch.tv_archive) {
      setOverlay(ch.name, p.title || '', 'not yet available — try again later');
      return;
    }
    if (isPast) {
      setOverlay(ch.name, p.title || '', 'not available');
      return;
    }
    // Future program.
    setOverlay(ch.name, p.title || '', 'not aired yet');
    return;
  }
  var items = visibleItems();
  var it = items[state.focusIdx];
  if (!it) return;
  if (it.kind === 'recent') {
    state.search = it.text;
    state.focusIdx = 0;
    render();
    return;
  }
  // In mini mode, OK on the currently-playing channel goes back to fullscreen without
  // restarting playback. OK on a different channel switches and goes fullscreen.
  if (state.mini && state.playing && state.playing.channel.key === it.channel.key) {
    captureZapList();
    state.mini = false;
    attachVideoForMode();
    updateBodyClass();
    setOverlay(it.channel.name, '', 'fullscreen');
    return;
  }
  play(it.channel);
}

function back() {
  if (state.playing) {
    // First back: shrink fullscreen → corner mini so the user can keep watching
    // while browsing. Second back: stop entirely.
    if (!state.mini) {
      state.mini = true;
      if (zapTimer) { clearTimeout(zapTimer); zapTimer = null; }
      // Settings isn't reachable while mini — if that's where the right-col was
      // parked, retire to the list so OK doesn't accidentally fire a setting.
      if (state.panel === 'settings') state.panel = 'list';
      updateBodyClass();
      hideOverlay();
      render();
      return;
    }
    // In mini, peel off the open search first so the user can exit search without
    // killing the corner playback. Another back then stops playback.
    if (state.search != null) {
      state.search = null;
      state.focusIdx = 0;
      render();
      return;
    }
    player.stop();
    state.playing = null;
    state.mini = false;
    state.zapList = null;
    updateBodyClass();
    hideOverlay();
    render();
    return;
  }
  if (state.search != null) {
    state.search = null;
    state.focusIdx = 0;
    render();
  }
}

function toggleSearch() {
  if (state.playing && !state.mini) return;
  state.search = (state.search == null) ? '' : null;
  state.focusIdx = 0;
  // Snap navigation back to the list — otherwise arrows/OK keep operating on the
  // settings or EPG panel while the user is typing into the search box.
  if (state.search != null) {
    state.panel = 'list';
    focusSearchOnNextRender = true;
  }
  render();
}

// Rewind from live into a fresh catch-up VOD chunk. Anchor is "now + offsetSec" (passed
// as a small negative number, typically −30 s for ◁ and −60 s for Blue). 60-min default
// chunk so subsequent seeks stay native within the playlist.
//
// Caveat: the provider's encoder lag (~2.7h on RTP 1) means very-recent anchors will
// 404 in practice until the slot is archived. Phase 3e surfaces that as a toast.
function enterCatchupAtNow(offsetSec) {
  if (!state.playing) return;
  var ch = state.playing.channel;
  if (!ch.tv_archive) {
    setOverlay(ch.name, '', 'no catch-up on this channel');
    return;
  }
  var atIso = new Date(Date.now() + offsetSec * 1000).toISOString();
  var url = catchupUrl(ch, atIso, 60);
  state.playing = {
    channel: ch,
    mode: 'catchup',
    catchup: { atIso: atIso, durationMin: 60 }
  };
  updateBodyClass();
  showCatchupBadge();
  setOverlay(ch.name, '', 'CATCH-UP · rewinding…', true);
  logEvent('catchup-rewind', ch.name, { url: url, offsetSec: offsetSec });
  player.play(url);
}

// Blue key while playing: toggle between live and catch-up for the current channel.
//   - catchup → swap src back to the live URL ("back to LIVE")
//   - live    → rewind 60 s into the catch-up window (same semantics as ◁, with a
//                slightly longer offset since Blue is the "deliberate jump" key)
// Outside playback, Blue is a no-op; the EPG panel has its own OK-on-replay-row entry.
function toggleCatchupMode() {
  if (!state.playing) return;
  var ch = state.playing.channel;
  if (state.playing.mode === 'catchup') {
    state.playing = { channel: ch, mode: 'live' };
    updateBodyClass();
    setOverlay(ch.name, '', 'live', true);
    logEvent('catchup-exit', ch.name);
    player.play(ch.play_url);
    return;
  }
  enterCatchupAtNow(-60);
}

// Red key: drop the focused channel from the recents list. After removal it falls back
// to its default-order position. Focus stays at the same index so repeated red presses
// peel recents off one by one from the top.
function unrecent() {
  if ((state.playing && !state.mini) || state.panel === 'settings') return;
  var items = visibleItems();
  var it = items[state.focusIdx];
  if (!it || it.kind !== 'channel' || !it.played) return;
  if (!removeRecentChannel(it.channel.key)) return;
  setOverlay(it.channel.name, '', 'removed from recents');
  render();
}

// Fullscreen key-hint legend. The channel-list view shows a static legend in the header,
// but during fullscreen playback the app shell is hidden — users can't see it. Solution:
// flash a context-aware legend (live vs catch-up; tv_archive vs not) on any key press
// and auto-hide 3 s after the last key.
var legendTimer = null;
function showLegend() {
  if (!state.playing || state.mini) return;
  var el = document.getElementById('legend');
  if (!el) {
    el = document.createElement('div');
    el.id = 'legend';
    document.body.appendChild(el);
  }
  el.innerHTML = legendContent();
  el.classList.add('visible');
  if (legendTimer) clearTimeout(legendTimer);
  legendTimer = setTimeout(function () { el.classList.remove('visible'); }, 3000);
}

function legendContent() {
  var mode = state.playing && state.playing.mode;
  var ch = state.playing && state.playing.channel;
  var supportsCatchup = !!(ch && ch.tv_archive);
  var label, rows = [];
  if (mode === 'catchup') {
    label = 'CATCH-UP';
    rows.push('<span><span class="k">◁▷</span>seek ±30s</span>');
    rows.push('<span><span class="k k-b">B</span>back to LIVE</span>');
    rows.push('<span><span class="k">△▽</span>channel</span>');
    rows.push('<span><span class="k">Back</span>mini</span>');
  } else {
    label = 'LIVE';
    if (supportsCatchup) {
      rows.push('<span><span class="k">◁</span>catch-up −30s</span>');
      rows.push('<span><span class="k k-b">B</span>catch-up −60s</span>');
    } else {
      rows.push('<span class="dim"><span class="k">◁</span>no catch-up</span>');
    }
    rows.push('<span><span class="k k-g">G</span>next source</span>');
    rows.push('<span><span class="k">△▽</span>channel</span>');
    rows.push('<span><span class="k">Back</span>mini</span>');
  }
  return '<div class="lbl">' + esc(label) + ' CONTROLS</div><div class="row">' + rows.join('') + '</div>';
}

var overlayTimer = null;
function setOverlay(title, subtitle, status, persist) {
  var ov = document.getElementById('overlay');
  if (!ov) {
    ov = document.createElement('div');
    ov.id = 'overlay';
    document.body.appendChild(ov);
  }
  ov.innerHTML =
    '<div class="ov-title">' + esc(title) + '</div>' +
    (subtitle ? '<div class="ov-sub">' + esc(subtitle) + '</div>' : '') +
    (status ? '<div class="ov-status">' + esc(status) + '</div>' : '');
  ov.style.opacity = '1';
  if (overlayTimer) clearTimeout(overlayTimer);
  if (!persist) overlayTimer = setTimeout(function () { ov.style.opacity = '0'; }, 2500);
}
function hideOverlay() {
  var ov = document.getElementById('overlay');
  if (ov) ov.style.opacity = '0';
  if (overlayTimer) clearTimeout(overlayTimer);
}

player.onPlaying = function (url) {
  mark('firstPlaying');
  if (state.playing) {
    setOverlay(state.playing.channel.name, '', 'live');
    logEvent('canplay', state.playing.channel.name, { url: url });
  }
  // The player just appended a fresh <video> to document.body. If we're in mini mode
  // we must re-parent it into #top-slot in the same synchronous tick — otherwise the
  // body.mini #player CSS (position:absolute; inset:0) paints it fullscreen for a frame.
  attachVideoForMode();
};
player.onSourceFailed = function (url, reason) {
  if (!state.playing) return;
  if (state.playing.mode === 'catchup') {
    // Catch-up has one upstream per channel (the tv_archive=1 source). The proxy bypasses
    // the live blacklist for catch-up requests, so a failure here means the catch-up
    // source itself is genuinely down — nothing to fail over to. Surface and stop.
    var ch = state.playing.channel;
    logEvent('catchup-fail', ch.name, { url: url, reason: reason });
    setOverlay(ch.name, '', 'catch-up unavailable — try again later');
    player.stop();
    state.playing = null;
    state.mini = false;
    state.zapList = null;
    updateBodyClass();
    render();
    return;
  }
  logEvent('fail', state.playing.channel.name, { url: url, reason: reason });
  // Await the POST before refreshing — otherwise the refresh can race the
  // blacklist update and the server may serve the same broken URL again.
  setOverlay(state.playing.channel.name, '', 'retrying…', true);
  reportFailure(state.playing.channel.key, reason).then(function () {
    player.refresh();
  });
};

initRemote();
setRemoteHandlers({
  arrowUp: function () { moveFocus(-1); },
  arrowDown: function () { moveFocus(1); },
  arrowLeft: function () { moveHorizontal(-1); },
  arrowRight: function () { moveHorizontal(1); },
  ok: function () { activate(); },
  back: function () { back(); },
  yellow: function () { toggleSearch(); },
  green: function () { if (state.playing && state.playing.mode === 'live') switchSource(); },
  blue: function () { toggleCatchupMode(); },
  red: function () { unrecent(); },
  channelUp: function () { (state.playing && !state.mini) ? zap(-1) : moveFocus(-1); },
  channelDown: function () { (state.playing && !state.mini) ? zap(1) : moveFocus(1); },
  home: function () { jumpToEdge(false); },
  end: function () { jumpToEdge(true); },
  any: function () { showLegend(); showCatchupBadge(); }
});

// === BOOT ===

// Step 1: instant paint from the cached /api/channels response (no spinner).
var cachedChannels = loadChannelsCache();
if (cachedChannels && cachedChannels.length) {
  state.channels = cachedChannels;
  mark('cachedRender');
}
render();

// Step 2: fetch the fresh catalog, save it, re-render.
catalogLoading = true;
mark('channelsFetchStart');
listChannels().then(function (channels) {
  mark('channelsFetchEnd');
  catalogLoading = false;
  state.channels = channels;
  saveChannelsCache(channels);
  state.zapList = null;
  render();
}).catch(function (err) {
  catalogLoading = false;
  if (!state.channels.length) {
    state.error = 'proxy fetch failed: ' + (err && err.message || err);
    render();
  }
});

// Step 3: poll /api/status for the header host count. Same cadence as the old probe
// loop's settle time — fast first poll, then steady.
function pollStatus() {
  return getStatus().then(function (s) {
    serverStatus = s;
    window.__app.status = s;
    render();
  }).catch(function () { /* ignore — keep stale value */ });
}
pollStatus();
setInterval(pollStatus, 15000);
