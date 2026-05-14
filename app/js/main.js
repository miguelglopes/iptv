import { initRemote, setRemoteHandlers } from './remote.js';
import { Player } from './player.js';
import {
  loadSearchHistory, pushSearchHistory, clearSearchHistory,
  loadRecentChannels, pushRecentChannel, removeRecentChannel, clearRecentChannels,
  loadChannelsCache, saveChannelsCache, clearChannelsCache,
  loadEpg, saveEpg,
  loadLastPlayTimestamp
} from './cache.js';
import { listChannels, epgFor, reportFailure, demoteSource, getStatus, adminReprobe, adminClearBlacklist, adminClearDemoted, adminClearAllSources } from './api.js';

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
// encoding them yet. Empirically ~2.7h on RTP 1.
var PROVIDER_LAG_MS = 3 * 3600 * 1000;

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
  var items = visibleItems();
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
  scrollIntoCenter(el, list);
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
  var prevL = state.focusIdx;
  state.focusIdx = nextL;
  var listEl = document.getElementById('list');
  if (listEl) {
    var prevElL = listEl.children[prevL];
    var nextElL = listEl.children[nextL];
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
  state.focusIdx = end ? list.length - 1 : 0;
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
  pushRecentChannel(channel.key);
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
  var recents = loadRecentChannels();
  if (!recents.length) return;
  var lastTs = loadLastPlayTimestamp();
  if (!lastTs || (Date.now() - lastTs) > AUTO_RESUME_MAX_AGE_MS) return;
  var targetKey = recents[0];
  var ch = null;
  for (var i = 0; i < state.channels.length; i++) {
    if (state.channels[i].key === targetKey) { ch = state.channels[i]; break; }
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
  pushRecentChannel(channel.key);
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
  state.playing = { channel: ch, mode: 'live' };
  updateBodyClass();
  setOverlay(ch.name, '', '…', true);
  clearTimeout(zapTimer);
  zapTimer = setTimeout(function () {
    zapTimer = null;
    if (state.playing) {
      pushRecentChannel(state.playing.channel.key);
      player.play(state.playing.channel.play_url);
    }
  }, 250);
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
      state.mini = true;
      clearTimeout(zapTimer); zapTimer = null;
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
  if (!removeRecentChannel(it.channel.key)) return;
  setOverlay(it.channel.name, '', 'removed from recents');
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
    setOverlay(state.playing.channel.name, '', 'live');
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
  any: function () {
    userInteracted = true;
    showLegend();
    showCatchupBadge();
    // Cursor hide-on-key: keyboard activity hides the cursor; mousemove restores it.
    // Mutually exclusive with .pointer-active so the two handlers don't fight.
    var c = document.body.classList;
    c.add('no-cursor');
    c.remove('pointer-active');
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
  // Fullscreen playback: nothing in the shell is interactive; tap on the video
  // shrinks to mini (mobile equivalent of the remote's Back).
  if (state.playing && !state.mini) {
    if (e.target.id === 'player') back();
    return;
  }
  // Header key-hint chips → same actions the colored remote keys trigger.
  var chip = e.target.closest('.keymap-inline .chip[data-action]');
  if (chip) {
    var action = chip.getAttribute('data-action');
    if (action === 'search') toggleSearch();
    else if (action === 'unrecent') unrecent();
    else if (action === 'ok') activate();
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
    scheduleEpgFetch();
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
