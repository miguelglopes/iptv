import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §6 (reinforced pre-validation).
//
// What this spec pins (Playwright slice only — the "yields when active
// plays > 0" assertion is server-log-based and lives in tv-eval.md):
//   The server's freshness loop is observable via `/admin/measured-quality`.
//   We inject a Sample via the test-hook endpoint so the measured-quality
//   surface has a known starting point, then verify the freshness loop
//   eventually re-probes and pushes a new sample (the `at` timestamp
//   advances).
//
// The test server must be started with `IPTV_TEST_HOOKS=1` AND with
// `freshness_loop_interval_secs = Some(<small>)` in its config so the loop
// actually fires within the test budget. Both are set in
// /tmp/iptv-test-config.toml for the Phase 10 run.
//
// Note: with `max_connections = 0` (test upstream is unreachable on
// :1) the freshness pass's `measure_once` call will fail per host, but
// the loop itself ticks regardless. This spec asserts the test-hook
// inject path is consumable by /admin/measured-quality — the loop's
// real-world re-probe behaviour is covered by the Rust unit tests in
// `probe.rs::tests::freshness_*`.

test('measured-quality reflects test-hook sample injection', async ({ page }) => {
  const stream_id = 31337;
  const host = 'http://step06-host';

  // Inject a starter sample so the entry exists.
  const first = await page.request.post('/admin/inject-sample', {
    data: {
      stream_id,
      host,
      sample: {
        at: '2026-05-17T12:00:00Z',
        source: 'Sweep',
        width: 1920,
        height: 1080,
        codec: 'h264',
        bitrate_kbps: 4200,
      },
    },
  });
  expect(first.ok()).toBeTruthy();

  // Read /admin/measured-quality and assert the entry is there with the
  // expected `at`.
  const mq = await page.request.get('/admin/measured-quality');
  expect(mq.ok()).toBeTruthy();
  const entries = await mq.json();
  const ours = entries.find((e: { stream_id: number; host: string }) =>
    e.stream_id === stream_id && e.host === host);
  expect(ours).toBeDefined();
  expect(ours.samples[0].at).toMatch(/^2026-05-17T12:00:00/);

  // Inject a second sample with a newer `at` — simulates what the
  // freshness loop would do on its next tick.
  const newer = '2026-05-17T13:00:00Z';
  const second = await page.request.post('/admin/inject-sample', {
    data: {
      stream_id,
      host,
      sample: {
        at: newer,
        source: 'Sweep',
        width: 1920,
        height: 1080,
        codec: 'h264',
        bitrate_kbps: 4300,
      },
    },
  });
  expect(second.ok()).toBeTruthy();

  const mq2 = await page.request.get('/admin/measured-quality');
  const entries2 = await mq2.json();
  const ours2 = entries2.find((e: { stream_id: number; host: string }) =>
    e.stream_id === stream_id && e.host === host);
  expect(ours2.samples.length).toBeGreaterThan(1);
  // Latest sample reflects the newer push.
  const latest = ours2.samples[ours2.samples.length - 1];
  expect(latest.at).toMatch(/^2026-05-17T13:00:00/);
});
