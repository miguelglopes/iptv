// Mounted over app/js/config.js inside the Docker container (see docker-compose.yml).
// UI and proxy are co-located on the same process/port, so leave PROXY_BASE_URL
// empty — the browser then issues same-origin fetches against whatever host:port
// served the page (LAN IP, public IP, reverse proxy, …) without any per-host
// configuration. The host's gitignored app/js/config.js is unchanged and continues
// to drive the TV / laptop-dev flows.
export var PROXY_BASE_URL = '';
export var PROVIDER_LAG_MS = 0;
