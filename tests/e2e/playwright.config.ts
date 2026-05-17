import { defineConfig, devices } from '@playwright/test';

// Single-worker against a local `docker compose up` (or `cargo run`) of the
// proxy on :8080. Serial because some specs mutate server-side state
// (measured cache, cool-off) and parallel runs would race.
//
// The repo's `app/js/config.js` is gitignored and typically points at the
// production proxy URL — that would break every `page.route()` mock via
// CORS. `tests/e2e/_helpers.ts::useLocalConfig(page)` is the canonical
// way to redirect API traffic to the page origin during tests; every
// spec calls it from a `test.beforeEach`. No host-side `app/js/config.js`
// swap is needed (or wanted — that would leak between dev sessions).
export default defineConfig({
  testDir: '.',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 30_000,
  expect: { timeout: 5_000 },
  reporter: 'list',
  use: {
    baseURL: 'http://localhost:8080',
    trace: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
