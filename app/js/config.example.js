// Copy this file to config.js and adjust to your network.
// The TV resolves this URL itself, so it must be reachable from the TV's LAN.
export var PROXY_BASE_URL = 'http://192.168.1.10:8080';

// Catch-up archive lag of your provider, in milliseconds.
// 0 means trust EPG timestamps as-is — programs become catch-up the moment they
// end. If your provider's encoder is slow and catch-up shows "not yet available"
// for content you know finished airing, raise this to mask the gap. Empirical:
// ~3h (10800000) for some Portuguese hosts on RTP.
export var PROVIDER_LAG_MS = 0;
