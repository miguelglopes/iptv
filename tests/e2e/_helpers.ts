import { Page } from '@playwright/test';

// Canonical test-side override for `app/js/config.js`. The repo's
// `app/js/config.js` is gitignored and normally points at the production
// proxy URL — a cross-origin host would break every `page.route('**/...')`
// mock via CORS. Each spec calls `useLocalConfig(page)` in `beforeEach`
// (or at the top of the test) so the served-by-the-app config redirects
// API traffic back to whatever Playwright's `baseURL` is, keeping mocks
// same-origin without any host-side file swap.
//
// Both port 8080 (default deployment) and 8081 (test-server-side-by-side
// with the user's dev session) are valid Playwright baseURLs; `''` makes
// the client resolve API paths relative to the page origin, which IS the
// Playwright baseURL — same effect, no port to keep in sync.
export async function useLocalConfig(page: Page) {
  await page.route('**/js/config.js', (route) =>
    route.fulfill({
      headers: { 'content-type': 'application/javascript' },
      body:
        '// Synthesised by tests/e2e/_helpers.ts::useLocalConfig — overrides\n' +
        '// the gitignored app/js/config.js for the duration of one Playwright\n' +
        '// page. Empty PROXY_BASE_URL means the client uses the page origin,\n' +
        '// which Playwright sets to its baseURL.\n' +
        'export var PROXY_BASE_URL = "";\n' +
        'export var PROVIDER_LAG_MS = 0;\n',
    }),
  );
}
