import { initRemote, setRemoteHandlers } from './remote.js';
import { Player } from './player.js';
import {
  loadSearchHistory, pushSearchHistory, clearSearchHistory,
  loadRecentChannels, pushRecentChannel, removeRecentChannel, clearRecentChannels,
  loadChannelsCache, saveChannelsCache, clearChannelsCache,
  loadEpg, saveEpg,
  loadLastPlayTimestamp,
  loadActiveMode, saveActiveMode
} from './cache.js';
import { listChannels, epgFor, reportFailure, demoteSource, getStatus, adminReprobe, adminClearBlacklist, adminClearDemoted, adminClearAllSources } from './api.js';
// Namespace import so the field stays optional — older config.js files that pre-date
// PROVIDER_LAG_MS won't crash with a named-import SyntaxError; we just fall back to 0.
import * as cfg from './config.js';

// Tag the root element so TV-specific CSS (cursor: none, anything else added later) can
// gate on the real TV vs. laptop dev. webOS UA contains "Web0S" (yes, zero, not O).
if (/Web0S/i.test(navigator.userAgent)) document.documentElement.classList.add('tv');

var player = new Player();

// SVG "replay" glyph. webOS Chromium's default fonts ship without U+21BB, so the
// literal ↻ character renders as a tofu square on the TV. Inline SVG dodges that
// entirely — uses currentColor so callers inherit text colour. Size set in CSS
// per consumer (.replay-icon defaults; specific contexts override width/height).
var REPLAY_SVG = '<svg class="replay-icon" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 5V1L7 6l5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6H4c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/></svg>';

var state = {
  view: 'channels',       // 'channels' | 'playing'
  channels: [],           // [{ key, name, kind, logo?, default_rank?, source_count, play_url }]
  mode: loadActiveMode(), // 'tv' | 'radio' — filters `channels` by kind. Persisted so boot
                          // resumes into the last-used mode (see tryAutoResume).
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
  c.toggle('searching', state.search != null);
  c.toggle('error', !!state.error);
  updateCatchupBadge();
}

// Make the catch-up badge briefly visible. 5 s auto-hide; re-fires on any remote key
// (see `any` handler) so the user can always confirm what mode they're in by touching
// the remote.
var catchupBadgeTimer = null;
function showCatchupBadge() {
  if (!state.playing || state.playing.mode !== 'catchup' || state.mini) return;
  var el = document.getElementById('catchup-badge');
  if (!el) return;
  el.classList.add('visible');
  clearTimeout(catchupBadgeTimer);
  catchupBadgeTimer = setTimeout(function () { el.classList.remove('visible'); }, 5000);
}

function updateCatchupBadge() {
  var el = document.getElementById('catchup-badge');
  if (!el) return;
  if (!state.playing || state.playing.mode !== 'catchup') {
    el.innerHTML = '';
    el.classList.remove('visible');
    clearTimeout(catchupBadgeTimer);
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
    '<span class="cu">' + REPLAY_SVG + ' CATCH-UP</span>' +
    '<span class="when">' + esc(when) + '</span>' +
    (title ? '<span class="ttl">— ' + esc(title) + '</span>' : '');
}

// Fast path for panel changes: only touch the body class, the focused list item, and
// the settings-item focus class. A full list rebuild would re-create 400+ DOM nodes
// which makes panel-cycling feel sluggish on the TV.
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
  if (state.search != null) renderList();
}

function clearRecentChannelsAction() {
  clearRecentChannels();
  notifyCleared('Recent channels');
  renderList();
}

function clearChannelsCacheAction() {
  clearChannelsCache();
  notifyCleared('Cached channels (next boot refetches)');
}

function clearAllAction() {
  try { localStorage.clear(); } catch (e) {}
  location.reload();
}

// Action lookup keyed by the `data-i` index in the static settings grid (index.html).
// Order must match the markup; if you change one, change the other.
var SETTINGS_ACTIONS = [
  resetBlockedSourcesAction,
  resetSourcePreferencesAction,
  resetAllSourceStateAction,
  reprobeAction,
  clearSearchHistoryAction,
  clearRecentChannelsAction,
  clearChannelsCacheAction,
  clearAllAction
];
var SETTINGS_COUNT = SETTINGS_ACTIONS.length;

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
// encoding them yet. Operator-specific — calibrate per provider via config.js.
var PROVIDER_LAG_MS = cfg.PROVIDER_LAG_MS || 0;

// Programs currently visible in the EPG panel for `ch`. Returns null if no data yet.
// The window is 1 h on regular channels (just anchor on "right now") and
// tv_archive_duration days on catch-up channels (so users can scroll into history).
// Cache the filtered list per channel for a minute; the cutoff drifts at most a few
// seconds between presses and the O(n) filter is wasted on hot-path navigation.
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

function nowEpgIndex(visible) {
  if (!visible) return -1;
  var now = Date.now();
  for (var i = 0; i < visible.length; i++) {
    var p = visible[i];
    if (p.start && p.end && p.start.getTime() <= now && now < p.end.getTime()) return i;
  }
  return -1;
}

// Pick the programme airing at `now`, plus the one just before and the one just after.
// Used by the zap banner to surface PREV / NOW / NEXT on the focused channel.
function prevNowNextPrograms(programs) {
  if (!programs || !programs.length) return { prev: null, now: null, next: null };
  var n = Date.now();
  var prev = null, now = null, next = null;
  for (var i = 0; i < programs.length; i++) {
    var p = programs[i];
    if (!p.start || !p.end) continue;
    var s = p.start.getTime(), e = p.end.getTime();
    if (s <= n && n < e) {
      now = p;
      for (var k = i - 1; k >= 0; k--) {
        var pp = programs[k];
        if (pp.end && pp.end.getTime() <= n) { prev = pp; break; }
      }
      for (var j = i + 1; j < programs.length; j++) {
        if (programs[j].start) { next = programs[j]; break; }
      }
      return { prev: prev, now: now, next: next };
    }
    if (p.end && p.end.getTime() <= n) prev = p;
    if (!next && s > n) next = p;
  }
  return { prev: prev, now: now, next: next };
}

function epgPanelHtml() {
  var ch = focusedChannel();
  if (!ch) return '<div class="epg-empty">no channel focused</div>';

  var head = '<div class="epg-channel">' + esc(ch.name);
  if (ch.tv_archive && ch.tv_archive_duration > 0) {
    head += '<span class="epg-catchup-pill">' + REPLAY_SVG + 'CATCH-UP · ' + ch.tv_archive_duration + 'd</span>';
  }
  head += '</div>';

  var entry = state.epg.byKey[ch.key];
  if ((!entry || !entry.programs) && entry && entry.fetching) return head + '<div class="epg-empty">loading…</div>';
  if (!entry || !entry.programs || !entry.programs.length) return head + '<div class="epg-empty">no schedule info</div>';

  var visible = visibleEpgPrograms(ch);
  if (!visible) return head + '<div class="epg-empty">no upcoming programs</div>';

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
        badge = '<div class="epg-replay-badge">' + REPLAY_SVG + '<span class="lbl">REPLAY</span></div>';
      } else if (ch.tv_archive && (now - p.end.getTime()) < PROVIDER_LAG_MS) {
        rowCls += ' past-pending';
        badge = '<div class="epg-pending-badge">' + REPLAY_SVG + 'soon</div>';
      } else {
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
  // never sees the previous channel's schedule. The full EPG render is deferred to
  // after the debounce, so rapid scrolling stays snappy.
  var ch = focusedChannel();
  if (ch && state.epg.focusKey !== ch.key) {
    state.epg.focusKey = ch.key;
    state.epg.rowIdx = -1;
    var panel = document.getElementById('bottom-slot');
    if (panel) panel.innerHTML = '<div class="epg-empty">loading…</div>';
  }
  clearTimeout(epgFetchTimer);
  epgFetchTimer = setTimeout(fetchEpgForFocused, 80);
}

function fetchEpgForFocused() {
  var ch = focusedChannel();
  if (!ch) return;

  var cached = loadEpg(ch.key);
  if (cached && cached.length) {
    state.epg.byKey[ch.key] = { programs: cached, fetching: false };
    renderEpg();
    return;
  }

  var entry = state.epg.byKey[ch.key];
  if (entry && entry.programs != null) {
    renderEpg();
    return;
  }

  state.epg.byKey[ch.key] = { programs: null, fetching: true };
  epgFor(ch.key).then(function (programs) {
    if (state.epg.focusKey !== ch.key) return;
    state.epg.byKey[ch.key] = { programs: programs, fetching: false };
    saveEpg(ch.key, programs);
    renderEpg();
  }).catch(function () {
    if (state.epg.focusKey !== ch.key) return;
    state.epg.byKey[ch.key] = { programs: [], fetching: false };
    saveEpg(ch.key, []);
    renderEpg();
  });
}

function renderEpg() {
  var panel = document.getElementById('bottom-slot');
  if (!panel) return;
  panel.innerHTML = epgPanelHtml();
  var focused = panel.querySelector('.epg-row.focused, .epg-row.focused-dim');
  var target = focused || panel.querySelector('.epg-row.now');
  if (target) scrollIntoCenter(target, panel);
}

function listHtml(items) {
  var html = '';
  var listActive = state.panel === 'list';
  for (var i = 0; i < items.length; i++) {
    var it = items[i];
    if (it.kind === 'header') {
      // Header sits as a sibling of .list-item but isn't navigable — no data-i, no
      // .list-item class. Click/hover delegates match `.list-item` only, so headers
      // are inert by construction. Keep the data-i attribute on channels so the
      // children[focusIdx] index lines up with the items[] array (headers occupy
      // their own slot too).
      html += '<div class="list-header" data-h="1">' + esc(it.text) + '</div>';
      continue;
    }
    var focus = (listActive && i === state.focusIdx) ? ' focused' : (i === state.focusIdx ? ' focused-dim' : '');
    if (it.kind === 'recent') {
      html += '<div class="list-item recent' + focus + '" data-i="' + i + '"><span class="ret">↩</span> ' + esc(it.text) + '</div>';
    } else {
      var played = it.played ? ' played' : '';
      var marker = it.inRecentsSection ? '<span class="recent-dot">●</span> ' : '';
      // Logo: <img loading="lazy" onerror="this.remove()">. Same template for
      // TV and radio. TV's ChannelDto.logo was already populated by the server
      // (from Xtream's stream_icon) and just sat unused — this renders it.
      // onerror = self-remove so dead URLs degrade to text-only without flicker.
      var logo = it.channel.logo
        ? '<img class="logo" src="' + esc(it.channel.logo) + '" loading="lazy" onerror="this.remove()">'
        : '';
      html += '<div class="list-item' + played + focus + '" data-i="' + i + '">' + marker + logo + esc(it.channel.name) + '</div>';
    }
  }
  return html;
}

// Channels are always shown in the server's default order (default_rank then alpha) —
// the list never reshuffles around recents. visibleItems() surfaces the recently-watched
// set as a separate section pinned to the top of the list, then the full catalogue
// underneath. The recent channels appear twice (once in each section); both rows route
// through the same activate() path so behaviour is identical.
var RECENTS_SECTION_MAX = 8;

// Filter state.channels by the active mode tab (TV or radio). The mode is the
// *only* gate between TV and radio rendering — every other rendering path
// (recents, search, list-item template, scroll, focus) is shared.
function inMode(c) {
  var kind = c.kind || 'tv';
  return kind === state.mode;
}

function visibleChannels() {
  var base = state.channels.filter(inMode);
  if (!state.search) return base;
  var s = state.search.toLowerCase();
  return base.filter(function (c) { return c.name.toLowerCase().indexOf(s) >= 0; });
}

// Switch mode tab. Resets focus to the top of the new list (otherwise focusIdx
// would point at an arbitrary row of the previously-active list). Persists the
// new mode so the next boot resumes into it. Closes any open search (the
// search history is shared across modes, so a typed query that was filtering
// TV channels would silently filter the radio list to zero matches).
function setMode(newMode) {
  if (newMode !== 'tv' && newMode !== 'radio') return;
  if (state.mode === newMode) return;
  state.mode = newMode;
  saveActiveMode(newMode);
  state.search = null;
  state.focusIdx = 0;
  state.epg.focusKey = null;
  state.epg.rowIdx = -1;
  updateModeTabs();
  updateBodyClass();
  renderList();
}

// Paint the .active class on whichever tab matches state.mode and refresh the
// per-tab channel counts. Called from renderList so the counts stay in sync
// with state.channels.
function updateModeTabs() {
  var tabs = document.querySelectorAll('.mode-tabs .tab');
  var counts = { tv: 0, radio: 0 };
  for (var i = 0; i < state.channels.length; i++) {
    var k = state.channels[i].kind || 'tv';
    counts[k] = (counts[k] || 0) + 1;
  }
  for (var t = 0; t < tabs.length; t++) {
    var m = tabs[t].getAttribute('data-mode');
    tabs[t].classList.toggle('active', m === state.mode);
    var cspan = tabs[t].querySelector('.count');
    if (cspan) cspan.textContent = counts[m] ? '(' + counts[m] + ')' : '';
  }
}

function visibleItems() {
  if (state.search === '') {
    var h = loadSearchHistory();
    if (h.length) return h.map(function (t) { return { kind: 'recent', text: t }; });
  }
  var recent = loadRecentChannels(state.mode);
  var played = {};
  for (var i = 0; i < recent.length; i++) played[recent[i]] = true;
  var channels = visibleChannels();
  var items = [];
  // Recents section: only when not actively filtering — during search the user is
  // looking for a specific channel, not browsing what they've watched.
  if (!state.search && recent.length) {
    var byKey = {};
    for (var x = 0; x < channels.length; x++) byKey[channels[x].key] = channels[x];
    var recentChannels = [];
    for (var j = 0; j < recent.length && recentChannels.length < RECENTS_SECTION_MAX; j++) {
      var rc = byKey[recent[j]];
      if (rc) recentChannels.push(rc);
    }
    if (recentChannels.length) {
      items.push({ kind: 'header', text: 'Recently watched' });
      for (var k = 0; k < recentChannels.length; k++) {
        items.push({ kind: 'channel', channel: recentChannels[k], played: true, inRecentsSection: true });
      }
      items.push({ kind: 'header', text: 'All channels' });
    }
  }
  for (var m = 0; m < channels.length; m++) {
    items.push({ kind: 'channel', channel: channels[m], played: !!played[channels[m].key] });
  }
  return items;
}

// Return the nearest non-header index from `from`, moving in direction `dir` (±1).
// -1 if we run off the edge.
function findNonHeader(items, from, dir) {
  var i = from;
  while (i >= 0 && i < items.length && items[i].kind === 'header') i += dir;
  return (i >= 0 && i < items.length) ? i : -1;
}

// Snap state.focusIdx onto a navigable row. Called from renderList so resets like
// `state.focusIdx = 0` (which would land on the Recently-watched header) never stick.
function validateFocus(items) {
  if (!items.length) { state.focusIdx = 0; return; }
  if (state.focusIdx < 0) state.focusIdx = 0;
  if (state.focusIdx >= items.length) state.focusIdx = items.length - 1;
  if (items[state.focusIdx].kind === 'header') {
    var fwd = findNonHeader(items, state.focusIdx, 1);
    var back = findNonHeader(items, state.focusIdx, -1);
    state.focusIdx = fwd >= 0 ? fwd : (back >= 0 ? back : 0);
  }
}

// Find a sensible focus row for a given channel key. Prefers the recents-section copy
// (so the just-played channel sits at the top of the list when returning to mini) and
// falls back to the all-channels copy.
function focusIdxForChannel(channelKey) {
  var items = visibleItems();
  for (var i = 0; i < items.length; i++) {
    var it = items[i];
    if (it.kind === 'channel' && it.channel.key === channelKey && it.inRecentsSection) return i;
  }
  for (var j = 0; j < items.length; j++) {
    var it2 = items[j];
    if (it2.kind === 'channel' && it2.channel.key === channelKey) return j;
  }
  return 0;
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

function hintText(visibleCount) {
  return visibleCount + (state.search ? ' / ' + state.channels.length : '') + ' · ' + hostStatusLabel();
}

function setHint(text) {
  var el = document.querySelector('.hint');
  if (el) el.innerHTML = text;
}

function renderList() {
  var listEl = document.getElementById('list');
  if (!listEl) return;
  updateModeTabs();
  var items = visibleItems();
  validateFocus(items);
  listEl.innerHTML = listHtml(items);
  setHint(hintText(items.length));
  scrollToFocus();
  // The static settings panel isn't rebuilt by renderList, so its .focused class
  // can otherwise leak across panel changes that bypass setPanel() (back / toggleSearch).
  // Re-sync each call: it's 8 toggles, cheap.
  var settingsItems = document.querySelectorAll('.settings-grid .settings-item');
  for (var si = 0; si < settingsItems.length; si++) {
    settingsItems[si].classList.toggle('focused', state.panel === 'settings' && si === state.settingsIdx);
  }
  if (state.panel === 'list') scheduleEpgFetch();
}

function setError(msg) {
  state.error = msg || '';
  var pre = document.querySelector('#error .error-msg');
  if (pre) pre.textContent = state.error;
  updateBodyClass();
}

// Manually scroll the immediate container to put `el` in the middle. Native
// element.scrollIntoView walks up to every scrollable ancestor (incl. body/html),
// which silently shifts the page even on overflow:hidden/clip parents in some
// engines. Manual scrollTop touches only the panel we mean to move.
function scrollIntoCenter(el, container) {
  if (!el || !container) return;
  var cr = container.getBoundingClientRect();
  var er = el.getBoundingClientRect();
  var offset = (er.top - cr.top) - (cr.height - er.height) / 2;
  container.scrollTop += offset;
}

function scrollIntoNearest(el, container) {
  if (!el || !container) return;
  var cr = container.getBoundingClientRect();
  var er = el.getBoundingClientRect();
  if (er.top < cr.top) container.scrollTop -= (cr.top - er.top);
  else if (er.bottom > cr.bottom) container.scrollTop += (er.bottom - cr.bottom);
}

function scrollToFocus() {
  var list = document.getElementById('list');
  if (!list) return;
  var el = list.querySelector('.list-item.focused');
  // Use nearest, not center: when the focused item is the first row of a section
  // (e.g., the top of Recently-watched), centering would push the section header
  // off-screen above the viewport. nearest only scrolls when actually needed, so
  // the header stays visible above the focused row.
  scrollIntoNearest(el, list);
}

function moveFocus(delta) {
  if (state.playing && !state.mini) return zap(delta);
  if (state.panel === 'settings') {
    var next = state.settingsIdx + delta;
    if (next >= SETTINGS_COUNT) {
      // Past the last item → drop into the EPG panel below.
      setPanel('epg');
      return;
    }
    if (next < 0) next = SETTINGS_COUNT - 1;
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
      if (delta < 0) {
        var topEmpty = state.mini ? 'player' : 'settings';
        if (topEmpty === 'settings') state.settingsIdx = SETTINGS_COUNT - 1;
        setPanel(topEmpty);
      }
      return;
    }
    var prevIdx = state.epg.rowIdx;
    if (prevIdx < 0) prevIdx = 0;
    var nextIdx = prevIdx + delta;
    if (nextIdx < 0) {
      var top = state.mini ? 'player' : 'settings';
      if (top === 'settings') state.settingsIdx = SETTINGS_COUNT - 1;
      setPanel(top);
      return;
    }
    if (nextIdx >= visible.length) nextIdx = visible.length - 1;
    state.epg.rowIdx = nextIdx;
    var panel = document.getElementById('bottom-slot');
    if (panel) {
      var prevEl = panel.querySelector('.epg-row[data-i="' + prevIdx + '"]');
      var nextEl = panel.querySelector('.epg-row[data-i="' + nextIdx + '"]');
      if (prevEl) prevEl.classList.remove('focused');
      if (nextEl) {
        nextEl.classList.add('focused');
        scrollIntoNearest(nextEl, panel);
      }
    }
    return;
  }
  var list = visibleItems();
  if (!list.length) return;
  // Clamp instead of wrapping — wrapping a 400-item list disorients the user.
  var nextL = state.focusIdx + delta;
  if (nextL < 0) nextL = 0;
  if (nextL >= list.length) nextL = list.length - 1;
  // Headers between sections aren't navigable — snap to the nearest channel in the
  // direction of motion (so △▽ never lands on a section heading), then fall back to
  // the opposite direction at the very edges.
  var dir = delta >= 0 ? 1 : -1;
  var snapped = findNonHeader(list, nextL, dir);
  if (snapped < 0) snapped = findNonHeader(list, nextL, -dir);
  if (snapped < 0) return;
  var prevL = state.focusIdx;
  state.focusIdx = snapped;
  var listEl = document.getElementById('list');
  if (listEl) {
    var prevElL = listEl.children[prevL];
    var nextElL = listEl.children[snapped];
    if (prevElL && prevElL !== nextElL) prevElL.classList.remove('focused');
    if (nextElL) {
      nextElL.classList.add('focused');
      scrollIntoNearest(nextElL, listEl);
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
  if (end) {
    var idx = findNonHeader(list, list.length - 1, -1);
    state.focusIdx = idx >= 0 ? idx : list.length - 1;
  } else {
    var idx2 = findNonHeader(list, 0, 1);
    state.focusIdx = idx2 >= 0 ? idx2 : 0;
  }
  renderList();
}

// Arrow left/right: timeline navigation while playing; panel switcher when idle.
function moveHorizontal(delta) {
  if (state.playing && !state.mini) {
    if (state.playing.mode === 'catchup') return seekCatchup(delta * 30);
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
      if (state.settingsIdx % 2 === 0 && state.settingsIdx + 1 < SETTINGS_COUNT) setSettingsIdx(state.settingsIdx + 1);
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
  pushRecentChannel(channel.key, state.mode);
  // The recents section was just bumped — re-anchor focus on this channel so it lines
  // up in the list when we come back to mini. Without this, state.focusIdx points to
  // wherever the click happened and now refers to a different row (the recents header
  // / a different channel) because everything shifted down.
  state.focusIdx = focusIdxForChannel(channel.key);
  state.playing = { channel: channel, mode: 'live' };
  state.mini = false;
  updateBodyClass();
  setOverlay(channel.name, '', 'connecting…', true);
  mark('playStart');
  logEvent('play', channel.name, { url: channel.play_url });
  player.play(channel.play_url);
}

// Auto-resume the most recently played channel on app boot.
var AUTO_RESUME_MAX_AGE_MS = 24 * 3600 * 1000;
var userInteracted = false;
var autoResumeTried = false;
function tryAutoResume() {
  if (autoResumeTried || userInteracted || state.playing || state.error) return;
  if (!state.channels.length) return;
  // state.mode was initialised from loadActiveMode() at state-init time, so the
  // recents we pull and the channel we resume into are both from the last
  // mode the user had open.
  var recents = loadRecentChannels(state.mode);
  if (!recents.length) return;
  var lastTs = loadLastPlayTimestamp(state.mode);
  if (!lastTs || (Date.now() - lastTs) > AUTO_RESUME_MAX_AGE_MS) return;
  var targetKey = recents[0];
  var ch = null;
  for (var i = 0; i < state.channels.length; i++) {
    // Filter by mode too so a radio canonical key that happens to collide with
    // a TV key can't auto-resume the wrong channel.
    var c = state.channels[i];
    if (c.key === targetKey && (c.kind || 'tv') === state.mode) { ch = c; break; }
  }
  if (!ch) return;
  autoResumeTried = true;
  logEvent('auto-resume', ch.name, { key: ch.key });
  state.focusIdx = 0;
  play(ch);
}

function catchupUrl(channel, atIso, durationMin) {
  var sep = channel.play_url.indexOf('?') >= 0 ? '&' : '?';
  return channel.play_url + sep +
    'at=' + encodeURIComponent(atIso) +
    '&duration=' + Math.max(1, Math.round(durationMin));
}

function playCatchup(channel, program) {
  if (!program || !program.start || !program.end) return;
  if (!channel.tv_archive) {
    setOverlay(channel.name, program.title || '', 'catch-up not supported on this channel');
    return;
  }
  captureZapList();
  pushRecentChannel(channel.key, state.mode);
  state.focusIdx = focusIdxForChannel(channel.key);
  var atIso = program.start.toISOString();
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

// Channel zap (only valid during fullscreen playback). Two phases so holding the key
// (or quickly tapping it several times) doesn't thrash the player pipeline:
//
//   1. Preview — each △▽ press updates `zapState` and the on-screen channel strip
//      but DOES NOT touch the <video> element. The actually-playing channel stays
//      visible underneath while the user scrolls through neighbouring channels.
//   2. Commit — fires after ZAP_COMMIT_MS of idle. Each new press restarts the
//      timer, so consecutive taps keep scrolling and only the final pause commits.
//      Promotes the preview channel to state.playing and calls player.play().
//
// state.zapList was captured in default order at fullscreen entry, so △▽ always
// follows the catalogue's sort — pinning a recent never reshuffles the zap order.
var zapState = null;
var zapTimer = null;
var commitHideTimer = null;
var ZAP_COMMIT_MS = 500;
var POST_COMMIT_LINGER_MS = 2000;

function zap(delta) {
  if (!state.playing) return;
  // Cancel any post-commit linger — we're starting a fresh preview cycle, the
  // strip should immediately reflect the new target rather than wait out the timer
  // and then snap.
  clearTimeout(commitHideTimer);
  commitHideTimer = null;
  var list = state.zapList || visibleChannels();
  if (!list.length) return;
  // Pivot off the in-progress preview during a hold so successive presses advance one
  // step each, not all jumping from the original channel.
  var pivotKey = zapState ? zapState.channel.key : state.playing.channel.key;
  var curIdx = -1;
  for (var i = 0; i < list.length; i++) {
    if (list[i].key === pivotKey) { curIdx = i; break; }
  }
  if (curIdx < 0) curIdx = 0;
  var nextIdx = (curIdx + delta + list.length) % list.length;
  zapState = { idx: nextIdx, channel: list[nextIdx], list: list };
  showZapPreview(list, nextIdx);
  showZapBanner(list[nextIdx]);
  scheduleZapEpgFetch(list[nextIdx]);
  hideOverlay();
  clearTimeout(zapTimer);
  zapTimer = setTimeout(commitZap, ZAP_COMMIT_MS);
}

function commitZap() {
  clearTimeout(zapTimer);
  zapTimer = null;
  if (!zapState) { hideZapPreview(); hideZapBanner(); return; }
  if (!state.playing) { zapState = null; hideZapPreview(); hideZapBanner(); return; }
  var ch = zapState.channel;
  var list = zapState.list;
  var idx = zapState.idx;
  zapState = null;
  state.playing = { channel: ch, mode: 'live' };
  // Order matters: pushRecentChannel BEFORE focusIdxForChannel, so the snap sees the
  // new recents-section row at the top and anchors there. If we snapped first, the
  // recents list would grow underneath us and state.focusIdx would point one row off.
  pushRecentChannel(ch.key, state.mode);
  state.focusIdx = focusIdxForChannel(ch.key);
  updateBodyClass();
  // No setOverlay here — the bottom-centre banner already carries channel name + LIVE,
  // and the post-commit linger keeps it visible. player.onPlaying is also suppressed
  // for the same reason while the banner is up.
  logEvent('zap-commit', ch.name, { url: ch.play_url });
  player.play(ch.play_url);
  // Linger: keep both the strip and the banner visible for POST_COMMIT_LINGER_MS so
  // the user has a confirmation of what they landed on. Re-render first — state.playing
  // is now the committed channel, which clears the LIVE pill from the row we just left.
  showZapPreview(list, idx);
  showZapBanner(ch);
  clearTimeout(commitHideTimer);
  commitHideTimer = setTimeout(function () {
    commitHideTimer = null;
    hideZapPreview();
    hideZapBanner();
  }, POST_COMMIT_LINGER_MS);
}

function cancelZap() {
  clearTimeout(zapTimer);
  zapTimer = null;
  clearTimeout(commitHideTimer);
  commitHideTimer = null;
  clearTimeout(zapEpgFetchTimer);
  zapEpgFetchTimer = null;
  zapState = null;
  hideZapPreview();
  hideZapBanner();
}

var ZAP_PREVIEW_WINDOW = 9;
function showZapPreview(list, idx) {
  var el = document.getElementById('zap-preview');
  if (!el) return;
  var half = Math.floor(ZAP_PREVIEW_WINDOW / 2);
  var start = idx - half;
  var end = idx + half;
  if (start < 0) { end += -start; start = 0; }
  if (end >= list.length) { start -= (end - list.length + 1); end = list.length - 1; }
  if (start < 0) start = 0;
  if (end >= list.length) end = list.length - 1;
  var html = '';
  for (var i = start; i <= end; i++) {
    var isCurrent = i === idx;
    var cls = isCurrent ? 'zap-row current' : 'zap-row dim';
    var isLive = state.playing && list[i].key === state.playing.channel.key;
    var tag = isLive && !isCurrent ? '<span class="zap-live">LIVE</span>' : '';
    html += '<div class="' + cls + '">' +
              '<div class="zap-name">' + tag + esc(list[i].name) + '</div>' +
            '</div>';
  }
  el.innerHTML = html;
  el.classList.add('visible');
}


function hideZapPreview() {
  var el = document.getElementById('zap-preview');
  if (el) el.classList.remove('visible');
}

// Bottom-centre channel-info banner shown alongside the right-side strip during
// fullscreen zap. Surfaces PREV / NOW / NEXT programmes for the focused channel,
// with a thin progress bar under NOW. Programme data comes from state.epg.byKey;
// the banner re-renders when fresh data arrives (refreshZapForChannel).
function showZapBanner(channel) {
  var el = document.getElementById('zap-banner');
  if (!el || !channel) return;
  var modeLabel = (state.playing && state.playing.mode === 'catchup')
    ? '<div class="zb-catchup">CATCH-UP</div>'
    : '<div class="zb-live">LIVE</div>';
  el.innerHTML =
    '<div class="zb-head">' +
      '<div class="zb-name">' + esc(channel.name) + '</div>' +
      modeLabel +
    '</div>' +
    zapBannerBodyHtml(channel);
  el.classList.add('visible');
}

function zapBannerBodyHtml(channel) {
  var entry = state.epg.byKey[channel.key];
  if (!entry || (entry.programs == null && entry.fetching)) {
    return '<div class="zb-msg">fetching schedule…</div>';
  }
  if (!entry.programs || !entry.programs.length) {
    return '<div class="zb-msg">no schedule info</div>';
  }
  var pnn = prevNowNextPrograms(entry.programs);
  if (!pnn.prev && !pnn.now && !pnn.next) {
    return '<div class="zb-msg">off air</div>';
  }
  var html = '';
  if (pnn.prev) html += zapBannerLine('prev', 'PREV', pnn.prev);
  if (pnn.now)  html += zapBannerLine('now', 'NOW', pnn.now) + zapBannerProgressBar(pnn.now);
  if (pnn.next) html += zapBannerLine('next', 'NEXT', pnn.next);
  return html;
}

function zapBannerLine(cls, label, p) {
  return '<div class="zb-line ' + cls + '">' +
           '<div class="zb-label">' + esc(label) + '</div>' +
           '<div class="zb-time">' + esc(fmtTime(p.start)) + ' – ' + esc(fmtTime(p.end)) + '</div>' +
           '<div class="zb-title">' + esc(p.title || '—') + '</div>' +
         '</div>';
}

function zapBannerProgressBar(p) {
  if (!p.start || !p.end) return '';
  var s = p.start.getTime(), e = p.end.getTime(), now = Date.now();
  var pct = (e > s) ? Math.max(0, Math.min(100, ((now - s) / (e - s)) * 100)) : 0;
  return '<div class="zb-bar"><div class="zb-bar-fill" style="width:' + pct.toFixed(1) + '%"></div></div>';
}

function hideZapBanner() {
  var el = document.getElementById('zap-banner');
  if (el) el.classList.remove('visible');
}

// On-demand EPG fetch for the zap target. Debounced so holding △▽ doesn't fire one
// request per row; only the channel the user lands on (180 ms idle) gets fetched.
// Results flow into the same state.epg.byKey cache used by the panel view, so subsequent
// zaps to that channel paint program info immediately. Cancelled by cancelZap().
var zapEpgFetchTimer = null;
function scheduleZapEpgFetch(channel) {
  if (!channel) return;
  clearTimeout(zapEpgFetchTimer);
  zapEpgFetchTimer = setTimeout(function () { fetchZapEpg(channel); }, 180);
}

function fetchZapEpg(channel) {
  if (!channel) return;
  var entry = state.epg.byKey[channel.key];
  if (entry && (entry.programs != null || entry.fetching)) {
    if (entry.programs != null) refreshZapForChannel(channel);
    return;
  }
  var cached = loadEpg(channel.key);
  if (cached && cached.length) {
    state.epg.byKey[channel.key] = { programs: cached, fetching: false };
    refreshZapForChannel(channel);
    return;
  }
  state.epg.byKey[channel.key] = { programs: null, fetching: true };
  refreshZapForChannel(channel);
  epgFor(channel.key).then(function (programs) {
    state.epg.byKey[channel.key] = { programs: programs, fetching: false };
    saveEpg(channel.key, programs);
    refreshZapForChannel(channel);
  }).catch(function () {
    state.epg.byKey[channel.key] = { programs: [], fetching: false };
    saveEpg(channel.key, []);
    refreshZapForChannel(channel);
  });
}

// Re-render the banner only if it's still showing this channel as the focus —
// a slow EPG response for a channel we've already zapped past would otherwise
// overwrite the correct preview. Works for both the live preview (zapState) and
// the post-commit linger (after commit, zapState is null but the banner is still up).
// The strip is channel-name-only and doesn't need re-rendering on EPG arrival.
function refreshZapForChannel(channel) {
  var el = document.getElementById('zap-banner');
  if (!el || !el.classList.contains('visible')) return;
  var targetKey;
  if (zapState) targetKey = zapState.channel.key;
  else if (state.playing) targetKey = state.playing.channel.key;
  else return;
  if (targetKey !== channel.key) return;
  showZapBanner(channel);
}

function seekCatchup(seconds) {
  if (!player.video) return;
  var v = player.video;
  var target = (v.currentTime || 0) + seconds;
  if (target < 0) target = 0;
  if (v.duration && isFinite(v.duration) && target > v.duration) target = v.duration;
  v.currentTime = target;
  logEvent('seek', String(seconds), { ct: v.currentTime, dur: v.duration });
  if (state.playing && state.playing.mode === 'catchup') {
    var ch = state.playing.channel;
    setOverlay(ch.name, '', 'CATCH-UP · ' + Math.round(v.currentTime) + ' / ' + (isFinite(v.duration) ? Math.round(v.duration) : '?') + 's', true);
  }
}

var switchTimer = null;
function switchSource() {
  if (!state.playing) return;
  // Mid-zap Green should refresh the currently-playing channel's source, not the
  // preview target — drop the preview so the user stays on what they're watching.
  cancelZap();
  setOverlay(state.playing.channel.name, '', 'switching source…', true);
  logEvent('switch', state.playing.channel.name);
  clearTimeout(switchTimer);
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
    var act = SETTINGS_ACTIONS[state.settingsIdx];
    if (act) act();
    return;
  }
  if (state.panel === 'player') {
    if (!state.playing) return;
    captureZapList();
    state.mini = false;
    state.panel = 'list';
    updateBodyClass();
    attachVideoForMode();
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
      play(ch);
      return;
    }
    if (isPast && ch.tv_archive && p.has_archive) {
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
    setOverlay(ch.name, p.title || '', 'not aired yet');
    return;
  }
  var items = visibleItems();
  var it = items[state.focusIdx];
  if (!it) return;
  if (it.kind === 'recent') {
    state.search = it.text;
    state.focusIdx = 0;
    var inp = document.getElementById('search');
    if (inp) inp.value = it.text;
    renderList();
    return;
  }
  // In mini mode, OK on the currently-playing channel goes back to fullscreen.
  if (state.mini && state.playing && state.playing.channel.key === it.channel.key) {
    captureZapList();
    state.mini = false;
    updateBodyClass();
    attachVideoForMode();
    setOverlay(it.channel.name, '', 'fullscreen');
    return;
  }
  play(it.channel);
}

function back() {
  if (state.playing) {
    if (!state.mini) {
      // Abandon any in-progress zap preview — the user is exiting fullscreen, not
      // confirming a channel switch.
      cancelZap();
      state.mini = true;
      if (state.panel === 'settings') state.panel = 'list';
      updateBodyClass();
      attachVideoForMode();
      hideOverlay();
      // Recents shifted when we entered fullscreen via play() — refresh the list so
      // returning to mini shows the new ordering / played state.
      renderList();
      return;
    }
    if (state.search != null) {
      state.search = null;
      state.focusIdx = 0;
      updateBodyClass();
      renderList();
      return;
    }
    player.stop();
    state.playing = null;
    state.mini = false;
    state.zapList = null;
    // 'player' / 'epg' are mini-only or playback-only contexts; once we've stopped,
    // they no longer make sense as the active panel and would lock the user out of
    // normal list navigation. Always land back on the list.
    if (state.panel !== 'list' && state.panel !== 'settings') state.panel = 'list';
    updateBodyClass();
    hideOverlay();
    renderList();
    return;
  }
  if (state.search != null) {
    state.search = null;
    state.focusIdx = 0;
    updateBodyClass();
    renderList();
  }
}

function toggleSearch() {
  if (state.playing && !state.mini) return;
  state.search = (state.search == null) ? '' : null;
  state.focusIdx = 0;
  if (state.search != null) {
    state.panel = 'list';
  }
  updateBodyClass();
  renderList();
  var inp = document.getElementById('search');
  if (inp) {
    if (state.search != null) {
      inp.value = '';
      inp.removeAttribute('readonly');
      inp.removeAttribute('tabindex');
      inp.classList.remove('exiled');
      inp.focus();
    } else {
      inp.value = '';
    }
  }
}

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

function toggleCatchupMode() {
  if (!state.playing) return;
  cancelZap();
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

function unrecent() {
  if ((state.playing && !state.mini) || state.panel === 'settings') return;
  var items = visibleItems();
  var it = items[state.focusIdx];
  if (!it || it.kind !== 'channel' || !it.played) return;
  var ch = it.channel;
  if (!removeRecentChannel(ch.key, state.mode)) return;
  // Stay anchored on the same conceptual channel — focusIdxForChannel falls back to
  // its all-channels row when the recents-section copy disappears.
  state.focusIdx = focusIdxForChannel(ch.key);
  setOverlay(ch.name, '', 'removed from recents');
  renderList();
}

// Fullscreen key-hint legend. Auto-hides 3 s after the last key.
var legendTimer = null;
function showLegend() {
  if (!state.playing || state.mini) return;
  var el = document.getElementById('legend');
  if (!el) return;
  el.innerHTML = legendContent();
  el.classList.add('visible');
  clearTimeout(legendTimer);
  legendTimer = setTimeout(function () { el.classList.remove('visible'); }, 3000);
}

function hideLegend() {
  var el = document.getElementById('legend');
  if (el) el.classList.remove('visible');
  clearTimeout(legendTimer);
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
  if (!ov) return;
  ov.innerHTML =
    '<div class="ov-title">' + esc(title) + '</div>' +
    (subtitle ? '<div class="ov-sub">' + esc(subtitle) + '</div>' : '') +
    (status ? '<div class="ov-status">' + esc(status) + '</div>' : '');
  ov.style.opacity = '1';
  clearTimeout(overlayTimer);
  if (!persist) overlayTimer = setTimeout(function () { ov.style.opacity = '0'; }, 2500);
}
function hideOverlay() {
  var ov = document.getElementById('overlay');
  if (ov) ov.style.opacity = '0';
  clearTimeout(overlayTimer);
}

player.onPlaying = function (url) {
  mark('firstPlaying');
  if (state.playing) {
    // Suppress the "live" overlay while the zap banner is up — the banner already
    // shows the channel name + LIVE pill. Avoids double-banner visual noise during
    // the post-commit linger.
    var banner = document.getElementById('zap-banner');
    if (!banner || !banner.classList.contains('visible')) {
      setOverlay(state.playing.channel.name, '', 'live');
    }
    logEvent('canplay', state.playing.channel.name, { url: url });
  }
  // The player just appended a fresh <video> to document.body. If we're in mini we
  // must re-parent it to track #top-slot in the same synchronous tick — otherwise the
  // default fullscreen CSS paints it edge-to-edge for a frame.
  attachVideoForMode();
};
player.onSourceFailed = function (url, reason) {
  if (!state.playing) return;
  if (state.playing.mode === 'catchup') {
    var ch = state.playing.channel;
    logEvent('catchup-fail', ch.name, { url: url, reason: reason });
    setOverlay(ch.name, '', 'catch-up unavailable — try again later');
    player.stop();
    state.playing = null;
    state.mini = false;
    state.zapList = null;
    if (state.panel !== 'list' && state.panel !== 'settings') state.panel = 'list';
    updateBodyClass();
    renderList();
    return;
  }
  logEvent('fail', state.playing.channel.name, { url: url, reason: reason });
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
  mode1: function () { setMode('tv'); },
  mode2: function () { setMode('radio'); },
  any: function () {
    userInteracted = true;
    // Suppress the bottom-right key legend while the zap banner is visible — they
    // share the bottom edge and would overlap. The user has the banner as their
    // focal point during zap; the legend reappears on the next key after the
    // banner fades.
    var banner = document.getElementById('zap-banner');
    if (!banner || !banner.classList.contains('visible')) showLegend();
    else hideLegend();
    showCatchupBadge();
    // Cursor hide-on-key: keyboard activity hides the cursor; mousemove restores it.
    // Mutually exclusive with .pointer-active so the two handlers don't fight.
    var c = document.body.classList;
    c.add('no-cursor');
    c.remove('pointer-active');
  },
  release: function () {
    // Intentionally a no-op for now. Earlier this committed an in-progress zap on
    // every keyup, but that meant a single tap → immediate channel change, which
    // made rapid tap-tap-tap thrash the player. ZAP_COMMIT_MS (500 ms idle) is now
    // the only commit path, so consecutive taps keep scrolling the preview and only
    // the final pause swaps the video.
  }
});

// Wire the search input once. The element is always in the DOM (hidden via
// body:not(.searching)) — only the value and visibility change.
(function () {
  var inp = document.getElementById('search');
  if (!inp) return;
  inp.addEventListener('input', function () {
    state.search = inp.value;
    state.focusIdx = 0;
    renderList();
  });
})();

// Touch / pointer support. The TV remote dispatches keydown events; this dispatches
// click events, so they don't collide. Mobile users tap; webOS Magic Remote pointer
// users get the same affordances on TV. Every path goes through the same activate()
// pipeline the keyboard uses.
document.addEventListener('click', function (e) {
  // Fullscreen playback: shell is hidden; clicking the video reveals the on-screen
  // legend (channel name, key hints, catch-up badge) — same overlay the keyboard
  // `any:` handler shows on a remote key. Back-out from fullscreen is keyboard-only.
  if (state.playing && !state.mini) {
    if (e.target.id === 'player') {
      showLegend();
      showCatchupBadge();
    }
    return;
  }
  // Mode-tabs tap → switch TV/RADIO. Same path the digit keys trigger.
  var tabEl = e.target.closest('.mode-tabs .tab[data-mode]');
  if (tabEl) {
    setMode(tabEl.getAttribute('data-mode'));
    return;
  }
  // Header key-hint chips → same actions the colored remote keys trigger.
  var chip = e.target.closest('.keymap-inline .chip[data-action]');
  if (chip) {
    var action = chip.getAttribute('data-action');
    if (action === 'search') toggleSearch();
    else if (action === 'unrecent') unrecent();
    else if (action === 'ok') activate();
    else if (action === 'mode') setMode(state.mode === 'tv' ? 'radio' : 'tv');
    return;
  }
  // Channel tap: focus that row and play.
  var li = e.target.closest('#list .list-item');
  if (li) {
    var i = parseInt(li.getAttribute('data-i'), 10);
    if (!isFinite(i)) return;
    if (state.panel !== 'list') setPanel('list');
    var listEl = document.getElementById('list');
    var prev = listEl.children[state.focusIdx];
    if (prev) { prev.classList.remove('focused'); prev.classList.remove('focused-dim'); }
    li.classList.add('focused');
    state.focusIdx = i;
    activate();
    return;
  }
  // Settings tap.
  var si = e.target.closest('.settings-grid .settings-item');
  if (si) {
    var idx = parseInt(si.getAttribute('data-i'), 10);
    if (!isFinite(idx)) return;
    if (state.panel !== 'settings') setPanel('settings');
    setSettingsIdx(idx);
    activate();
    return;
  }
  // EPG row tap.
  var epgEl = e.target.closest('.epg-row');
  if (epgEl) {
    var ei = parseInt(epgEl.getAttribute('data-i'), 10);
    if (!isFinite(ei)) return;
    if (state.panel !== 'epg') setPanel('epg');
    var panel = document.getElementById('bottom-slot');
    var prevRow = panel.querySelector('.epg-row.focused');
    if (prevRow) prevRow.classList.remove('focused');
    epgEl.classList.add('focused');
    state.epg.rowIdx = ei;
    activate();
    return;
  }
  // Mini: tap the player slot (or the <video> reparented over it) → maximize.
  if (state.mini && state.playing && (e.target.id === 'player' || e.target.closest('#top-slot'))) {
    state.panel = 'player';
    updateBodyClass();
    activate();
  }
});

// Pointer hover → focus. Mirrors arrow-key navigation: hovering a row sets the focus
// index, so OK (keyboard) and click (pointer) target the same item. Hover never
// activates — that's still a click. The hit-test caches the last target so we don't
// churn on every pixel of mouse movement.
var lastHoverTarget = null;
document.addEventListener('mousemove', function (e) {
  // Cursor reveal: pointer activity shows the cursor (overrides .no-cursor + TV default).
  var c = document.body.classList;
  c.remove('no-cursor');
  c.add('pointer-active');
  // Fullscreen playback: shell is hidden, nothing to hover.
  if (state.playing && !state.mini) return;
  var t = e.target.closest('#list .list-item, .settings-grid .settings-item, .epg-row');
  if (t === lastHoverTarget) return;
  lastHoverTarget = t;
  if (!t) return;
  if (t.classList.contains('list-item')) {
    var i = parseInt(t.getAttribute('data-i'), 10);
    if (!isFinite(i) || i === state.focusIdx) {
      if (state.panel !== 'list') setPanel('list');
      return;
    }
    if (state.panel !== 'list') setPanel('list');
    var listEl = document.getElementById('list');
    if (listEl) {
      var prev = listEl.children[state.focusIdx];
      if (prev && prev !== t) prev.classList.remove('focused');
      t.classList.add('focused');
    }
    state.focusIdx = i;
    // Don't scheduleEpgFetch() on hover — moving the cursor across the list would
    // flash the EPG panel "loading…" on every row crossed. EPG updates on keyboard
    // navigation (moveFocus) and on actual selection (click → play).
    return;
  }
  if (t.classList.contains('settings-item')) {
    var si = parseInt(t.getAttribute('data-i'), 10);
    if (!isFinite(si)) return;
    if (state.panel !== 'settings') setPanel('settings');
    setSettingsIdx(si);
    return;
  }
  if (t.classList.contains('epg-row')) {
    var ei = parseInt(t.getAttribute('data-i'), 10);
    if (!isFinite(ei)) return;
    if (state.panel !== 'epg') setPanel('epg');
    if (ei === state.epg.rowIdx) return;
    var panel = document.getElementById('bottom-slot');
    if (panel) {
      var prevRow = panel.querySelector('.epg-row.focused');
      if (prevRow && prevRow !== t) prevRow.classList.remove('focused');
      t.classList.add('focused');
    }
    state.epg.rowIdx = ei;
  }
});

// Wheel on the channel list: desktop uses native scroll. The TV's Magic Remote wheel
// has momentum and emits big deltaY values that overshoot a 412-item list; clamp the
// per-event step so it feels like a list, not a fling.
(function () {
  var listEl = document.getElementById('list');
  if (!listEl) return;
  listEl.addEventListener('wheel', function (e) {
    if (!document.documentElement.classList.contains('tv')) return;
    e.preventDefault();
    var step = Math.sign(e.deltaY) * Math.min(Math.abs(e.deltaY), 80);
    listEl.scrollBy({ top: step, behavior: 'auto' });
  }, { passive: false });
})();

// === BOOT ===

// Step 1: instant paint from the cached /api/channels response.
var cachedChannels = loadChannelsCache();
if (cachedChannels && cachedChannels.length) {
  state.channels = cachedChannels;
  mark('cachedRender');
}
updateBodyClass();
renderList();
tryAutoResume();

// Step 2: fetch the fresh catalog, save it, re-render.
catalogLoading = true;
mark('channelsFetchStart');
setHint(hintText(visibleItems().length));
listChannels().then(function (channels) {
  mark('channelsFetchEnd');
  catalogLoading = false;
  state.channels = channels;
  saveChannelsCache(channels);
  state.zapList = null;
  renderList();
  tryAutoResume();
}).catch(function (err) {
  catalogLoading = false;
  if (!state.channels.length) {
    setError('proxy fetch failed: ' + (err && err.message || err));
  }
});

// Step 3: poll /api/status for the header host count. Just updates the .hint text —
// no list rebuild.
function pollStatus() {
  return getStatus().then(function (s) {
    serverStatus = s;
    window.__app.status = s;
    setHint(hintText(visibleItems().length));
  }).catch(function () { /* ignore — keep stale value */ });
}
pollStatus();
setInterval(pollStatus, 15000);
