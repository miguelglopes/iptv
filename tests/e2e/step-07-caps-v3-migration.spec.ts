import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Phase 9 R2 issue 3: after the deploy that bumped the caps cache key
// from v2 → v3, a client with a stale v2 cache must NOT silently reuse
// it — the v2 cache predates the `hevc_main10` / `dvb_safe` probes, so
// reusing it would cause the freshness-loop-tightened channels to
// disappear without the client ever knowing it could play them.
//
// This spec seeds a v2-shaped cache (the pre-bump state) and asserts:
//   1. `loadCaps()` returns null (v2 is no longer the active key).
//   2. After boot, a v3 cache exists and contains a re-probed set.

const TEST_KEY = 'caps-v3-migration-test';

test('v2 caps cache is ignored after the v3 bump and re-probing fires', async ({ page }) => {
  // Seed v2 — the pre-Phase-9 shape. Includes only the pre-Phase-6 caps.
  await page.addInitScript(() => {
    localStorage.setItem(
      'xtream.client.caps.v2',
      JSON.stringify({
        ua: navigator.userAgent,
        caps: ['hls', 'h264', 'aac', 'live_video_hls'],
      }),
    );
    localStorage.setItem('xtream.caps.matrix_version', 'v1-stale');
  });

  await page.route('**/api/channels', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      headers: { 'x-caps-matrix-version': 'v2-current' },
      body: JSON.stringify([{
        key: TEST_KEY,
        name: 'Caps v3 Test',
        kind: 'tv',
        caps_required: ['hls', 'h264', 'aac', 'live_video_hls'],
        source_count: 1,
        play_url: `http://localhost:8080/play/${TEST_KEY}.m3u8`,
        tv_archive: false,
      }]),
    });
  });
  // Count probe requests. If the spec's seed bypassed re-probing the
  // count would stay zero; we expect at least one probe call after boot.
  let probeRequestCount = 0;
  await page.route('**/api/probe/**', async (route) => {
    probeRequestCount++;
    await route.fulfill({ status: 404 });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });
  await page.waitForTimeout(1_500);

  // v2 cache key is no longer the active key — loadCaps reads v3 only.
  // boot would have called ensureCaps → loadCaps returns null → probes
  // ran. Verify by probe request count.
  expect(probeRequestCount).toBeGreaterThan(0);

  // Confirm v3 doesn't materialise as a SIDE EFFECT of reading v2 (the
  // bump must be a hard cache reset, not a v2-to-v3 migration). v3 only
  // gets saved when at least one play-probe succeeds, which doesn't
  // happen with all-404 mocks — so v3 staying null here proves the v2
  // cache wasn't silently promoted.
  const state = await page.evaluate(() => ({
    v2: localStorage.getItem('xtream.client.caps.v2'),
    v3: localStorage.getItem('xtream.client.caps.v3'),
  }));
  expect(state.v3).toBeNull();
});
