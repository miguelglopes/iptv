import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §7 (per-channel caps_required +
// cap-matrix versioning).
//
// What this spec pins:
//   1. The server emits `X-Caps-Matrix-Version` on /api/channels.
//   2. If that version differs from what the client cached, the client
//      clears its cap cache, re-probes, and re-fetches /api/channels
//      before the next paint — without the operator manually clearing
//      anything.

const TEST_KEY = 'caps-matrix-test';

function stubChannel(playUrl: string, caps: string[]) {
  return [{
    key: TEST_KEY,
    name: 'Caps Matrix Test Channel',
    kind: 'tv',
    caps_required: caps,
    source_count: 1,
    play_url: playUrl,
    tv_archive: false,
  }];
}

test('matrix version header drives client re-probe on mismatch', async ({ page }) => {
  // Two phases of the /api/channels response: first ships v1, second ships
  // v2 (with the channel's caps tightened to require `hevc`). Probes 404
  // throughout so the client never picks up `hevc` on the second probe —
  // we just want to assert it RE-probed.
  let respIdx = 0;
  let probeRequestCount = 0;

  await page.route('**/api/channels', async (route) => {
    respIdx++;
    const version = respIdx === 1 ? 'v1-baseline' : 'v2-tightened';
    const caps = respIdx === 1
      ? ['hls', 'h264', 'aac', 'live_video_hls']
      : ['hls', 'hevc', 'aac', 'live_video_hls'];
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      headers: { 'x-caps-matrix-version': version },
      body: JSON.stringify(stubChannel(`http://localhost:8080/play/${TEST_KEY}.m3u8`, caps)),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    probeRequestCount++;
    await route.fulfill({ status: 404 });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });

  // After boot the client has cached v1. Verify probe ran at least once
  // (boot-time probe).
  const probesAfterBoot = probeRequestCount;
  expect(probesAfterBoot).toBeGreaterThan(0);
  expect(await page.evaluate(() => localStorage.getItem('xtream.caps.matrix_version')))
    .toBe('v1-baseline');

  // Reload — the next /api/channels response advertises v2-tightened. The
  // client's `listChannels` helper sees stored v1 vs server v2, kicks off
  // ensureCapsForMatrix (clear cache + re-probe) and re-fetches.
  await page.reload();
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });
  // Give the matrix-mismatch re-probe a moment to fire.
  await page.waitForTimeout(2_000);
  expect(probeRequestCount).toBeGreaterThan(probesAfterBoot);
  expect(await page.evaluate(() => localStorage.getItem('xtream.caps.matrix_version')))
    .toBe('v2-tightened');
});
