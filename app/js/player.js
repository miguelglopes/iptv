// Single-source player. The proxy fronts a single /play/<key>.m3u8 URL per channel
// and rotates upstreams server-side; this player just plays one URL and notifies the
// server on failure so the next playlist refresh picks a different upstream.

// Stall watchdog: webOS's demuxer can silently freeze on non-conformant TS streams
// (e.g. raw broadcast feeds with DVB subtitle PIDs) — readyState stays at 0, no
// `error` event ever fires. Without this, the channel just sits forever and the
// auto-retry path (which listens for `error`) never triggers. 10 s covers a slow
// upstream + segment fetch on the healthy path while still feeling snappy.
var STALL_WATCHDOG_MS = 10000;

export function Player() {
  this.video = null;
  this.url = null;
  this.onError = null;
  this.onPlaying = null;
  this.onSourceFailed = null;  // (url, reason) — fired on media error or stall
  this._watchdogTimer = null;
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
  v.addEventListener('playing', function () {
    self._clearWatchdog();
    if (self.onPlaying) self.onPlaying(self.url);
  });
  v.addEventListener('canplay', function () {
    self._clearWatchdog();
    if (self.onPlaying) self.onPlaying(self.url);
  });
  // loadeddata = readyState reached 2 = demuxer accepted at least one frame.
  // Once we're past that the stream is decodable; any later stall is normal
  // re-buffering territory, not the codec-mismatch case the watchdog catches.
  v.addEventListener('loadeddata', function () {
    self._clearWatchdog();
  });
  return v;
};

Player.prototype._tearDown = function () {
  this._clearWatchdog();
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

Player.prototype.play = function (url) {
  this.url = url;
  // webOS allows max-activated-media-players=1 — tear down the previous element first.
  this._tearDown();
  this.video = this._makeVideo();
  document.body.appendChild(this.video);
  this.video.src = url;
  this.video.load();
  this._armWatchdog();
  var p = this.video.play();
  if (p && p.catch) p.catch(function () {});
};

// User-initiated re-pick. The server will (post-feedback) pick a different upstream
// on the next /play/<key>.m3u8 request, so just refetch the same URL.
Player.prototype.refresh = function () {
  if (!this.url) return;
  // Bust any TV-level URL cache by appending a cache-buster.
  var url = this.url + (this.url.indexOf('?') >= 0 ? '&' : '?') + 't=' + Date.now();
  this._tearDown();
  this.video = this._makeVideo();
  document.body.appendChild(this.video);
  this.video.src = url;
  this.video.load();
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
