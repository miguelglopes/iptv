// Single-source player. The proxy fronts a single /play/<key>.m3u8 URL per channel
// and rotates upstreams server-side; this player just plays one URL and notifies the
// server on failure so the next playlist refresh picks a different upstream.
//
// Two playback paths (chosen at runtime by `window.Hls` presence):
//   - webOS Chromium (the LG TV): native HLS via `video.src = url`. Hardware
//     accelerated, low CPU. window.Hls is never defined there — see the
//     conditional <script> in index.html.
//   - Laptop / desktop browsers: hls.js demuxes client-side via MSE. Necessary
//     because Linux Chrome's native HLS demuxer is unreliable for live streams
//     (plays VOD HLS but stalls on EXT-X-MEDIA-SEQUENCE-tagged playlists, no
//     error event, just silent stall). hls.js handles both live and VOD.

// Stall watchdog: webOS's demuxer can silently freeze on non-conformant TS streams
// (e.g. raw broadcast feeds with DVB subtitle PIDs) — readyState stays at 0, no
// `error` event ever fires. Without this, the channel just sits forever and the
// auto-retry path (which listens for `error`) never triggers.
//
// Headroom: the proxy's exhaust-all candidate loop can take its full
// play_budget_secs (60 s default) to find a working upstream. The watchdog must
// be at least as long, otherwise we'd false-positive on a slow-but-eventually-
// successful failover. 70 s = budget + ~10 s for client-side demuxer to
// accept the first frame after the playlist response arrives.
var STALL_WATCHDOG_MS = 70000;

export function Player() {
  this.video = null;
  this.url = null;
  this.onError = null;
  this.onPlaying = null;
  this.onSourceFailed = null;  // (url, reason) — fired on media error or stall
  this._watchdogTimer = null;
  this._hls = null;  // hls.js instance when using the MSE path; null on webOS.
}

Player.prototype._makeVideo = function () {
  var v = document.createElement('video');
  v.autoplay = true;
  v.preload = 'auto';
  v.muted = false;
  v.id = 'player';
  // Styling lives in CSS (#player and body.mini #player) so the same element can be
  // re-parented into the mini slot without inline-style overrides fighting CSS.
  var self = this;
  v.addEventListener('error', function () {
    var reason = 'media error ' + (v.error && v.error.code);
    self._clearWatchdog();
    if (self.onSourceFailed) self.onSourceFailed(self.url, reason);
  });
  // canplay is the single "we're ready to play" signal: fires once the demuxer has
  // buffered enough to start. (Previously both `playing` and `canplay` were wired,
  // which double-fired onPlaying — extra setOverlay/log calls per play.)
  v.addEventListener('canplay', function () {
    self._clearWatchdog();
    if (self.onPlaying) self.onPlaying(self.url);
  });
  // loadeddata = readyState reached 2 = demuxer accepted at least one frame. Earlier
  // than canplay; clears the watchdog as soon as we know the stream is decodable.
  v.addEventListener('loadeddata', function () {
    self._clearWatchdog();
  });
  return v;
};

Player.prototype._tearDown = function () {
  this._clearWatchdog();
  if (this._hls) {
    try { this._hls.destroy(); } catch (e) {}
    this._hls = null;
  }
  if (this.video) {
    this.video.pause();
    this.video.removeAttribute('src');
    this.video.load();
    this.video.remove();
    this.video = null;
  }
};

Player.prototype._armWatchdog = function () {
  var self = this;
  this._clearWatchdog();
  this._watchdogTimer = setTimeout(function () {
    self._watchdogTimer = null;
    var v = self.video;
    if (!v) return;
    // Belt-and-braces: if the demuxer actually got somewhere we don't fire.
    if (v.readyState >= 2) return;
    var reason = 'stalled: no data after ' + (STALL_WATCHDOG_MS / 1000) + 's (rs=' + v.readyState + ' ns=' + v.networkState + ')';
    if (self.onSourceFailed) self.onSourceFailed(self.url, reason);
  }, STALL_WATCHDOG_MS);
};

Player.prototype._clearWatchdog = function () {
  if (this._watchdogTimer) {
    clearTimeout(this._watchdogTimer);
    this._watchdogTimer = null;
  }
};

// Hook either window.Hls (laptop) or video.src (webOS) to this.video.
// Same call site for play() and refresh().
Player.prototype._attachSource = function (url) {
  var self = this;
  var Hls = (typeof window !== 'undefined') && window.Hls;
  if (Hls && Hls.isSupported() && /\.m3u8(\?|$)/i.test(url)) {
    var hls = new Hls({
      // The proxy now exhausts all candidates within its play_budget_secs (60s
      // default) — let the manifest fetch take up to that long, otherwise hls.js
      // would abort while the proxy is still failing-over internally and we'd
      // report a fake failure. Per-fragment is shorter: a single segment that
      // can't load within 8s is genuinely broken (the proxy already
      // upstream-timed-out segments at the per_attempt cap).
      manifestLoadingTimeOut: 60000,
      fragLoadingTimeOut: 8000,
      enableWorker: true,
      lowLatencyMode: true,
    });
    hls.loadSource(url);
    hls.attachMedia(this.video);
    hls.on(Hls.Events.ERROR, function (_evt, data) {
      if (!data || !data.fatal) return;
      var reason = 'hls.js: ' + (data.details || data.type || 'fatal');
      if (self.onSourceFailed) self.onSourceFailed(self.url, reason);
    });
    this._hls = hls;
    return;
  }
  // webOS path (or any browser where hls.js isn't loaded / supported).
  this.video.src = url;
  this.video.load();
};

Player.prototype.play = function (url) {
  this.url = url;
  // webOS allows max-activated-media-players=1 — tear down the previous element first.
  this._tearDown();
  this.video = this._makeVideo();
  document.body.appendChild(this.video);
  this._attachSource(url);
  this._armWatchdog();
  var p = this.video.play();
  if (p && p.catch) p.catch(function () {});
};

Player.prototype.stop = function () {
  this.url = null;
  this._tearDown();
};

Player.prototype.isActive = function () {
  return this.video !== null;
};
