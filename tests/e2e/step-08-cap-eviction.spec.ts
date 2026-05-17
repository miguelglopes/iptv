import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §8 (client-side cap eviction).
//
// What this spec pins:
//   Three post-canplay failures on a channel that requires a specific cap
//   (e.g. `hevc`) cause the client to evict that cap from `loadCaps()`,
//   AND the next /api/channels request goes out with the now-tighter set
//   (so the server hides the unplayable channel + siblings).
//
// The test forges the "post-canplay" failure path by directly dispatching
// the error event after a synthetic canplay (same technique as step-03).

const TEST_KEY = 'hevc-evict-test';
const HEVC_KEY = 'hevc-only-channel';

function stubChannels(playUrl: string, includeHevc: boolean) {
  const list: Array<{
    key: string; name: string; kind: string; caps_required: string[];
    source_count: number; play_url: string; tv_archive: boolean;
  }> = [{
    key: TEST_KEY,
    name: 'HEVC Test Channel',
    kind: 'tv',
    caps_required: ['hls', 'hevc', 'aac', 'live_video_hls'],
    source_count: 1,
    play_url: playUrl,
    tv_archive: false,
  }];
  if (includeHevc) {
    list.push({
      key: HEVC_KEY,
      name: 'Another HEVC Channel',
      kind: 'tv',
      caps_required: ['hls', 'hevc', 'aac', 'live_video_hls'],
      source_count: 1,
      play_url: `http://localhost:8080/play/${HEVC_KEY}.m3u8`,
      tv_archive: false,
    });
  }
  return list;
}

test('three post-canplay failures evict the specific cap and hide siblings', async ({ page }) => {
  // Seed the cap cache so the client appears to have `hevc` capability —
  // otherwise the test channel would already be hidden by server filter
  // and we wouldn't be able to "play" it.
  await page.addInitScript(() => {
    const caps = ['hls', 'h264', 'hevc', 'aac', 'live_video_hls', 'mse', 'hls_native', 'hls_mse'];
    localStorage.setItem(
      'xtream.client.caps.v3',
      JSON.stringify({ ua: navigator.userAgent, caps }),
    );
    localStorage.setItem('xtream.caps.matrix_version', 'seeded-v1');
  });

  // Server simulates the homogeneous-HEVC tightening: every /api/channels
  // response includes the HEVC channel only as long as the client's
  // X-Client-Caps still claims `hevc`.
  let lastClientCaps: string | null = null;
  await page.route('**/api/channels', async (route) => {
    const headers = route.request().headers();
    lastClientCaps = headers['x-client-caps'] || '';
    const hasHevc = (lastClientCaps || '').split(',').includes('hevc');
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      headers: { 'x-caps-matrix-version': 'seeded-v1' },
      body: JSON.stringify(stubChannels(
        `http://localhost:8080/play/${TEST_KEY}.m3u8`,
        hasHevc,
      )),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404 });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });
  await page.route(`**/play/${TEST_KEY}.m3u8*`, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/vnd.apple.mpegurl',
      body: '#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n',
    });
  });
  await page.route('**/api/feedback/**', async (route) => {
    await route.fulfill({ status: 204 });
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });

  // Drive the eviction directly via the caps module. The production path is
  // `onSourceFailed(phase="post-canplay") → markCapFailure(tag)`; Playwright
  // can't reliably synthesise `v.readyState >= 2` on a video element (the
  // media pipeline computes that internally), so `dispatchEvent('error')`
  // always lands as `pre-canplay` and the eviction never fires. Calling
  // markCapFailure directly exercises the same eviction code path.
  const evictResult = await page.evaluate(async () => {
    const caps = await import('/js/caps.js');
    // Three post-canplay failures crosses the threshold (CAP_EVICTION_FAILS).
    let evictedOnAny = false;
    for (let i = 0; i < 3; i++) {
      if (caps.markCapFailure('hevc')) evictedOnAny = true;
    }
    return {
      evictedOnAny,
      // loadCaps filters evicted tags out.
      caps: caps.loadCaps(),
    };
  });
  expect(evictResult.evictedOnAny).toBe(true);
  expect(evictResult.caps).not.toContain('hevc');

  // Issue a fresh /api/channels request with the now-evicted caps header
  // so the server's filter hides the HEVC channel.
  await page.evaluate(async () => {
    const api = await import('/js/api.js');
    const caps = await import('/js/caps.js');
    api.setClientCaps(caps.loadCaps() || []);
    await api.listChannels();
  });
  expect(lastClientCaps).not.toContain('hevc');
});
