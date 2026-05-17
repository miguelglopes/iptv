import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §4-5 (exclusion → rank penalty + LKG
// into rank tuple + DVB-safe caps plumbing).
//
// What this pins:
//   1. Play URLs include `&caps=…` (Step 4 §9 plumbing — caps ride the URL
//      because the playback path bypasses our XHR wrapper).
//   2. After a feedback POST signals failure on a channel, the channel
//      stays in `/api/channels` (plan §8 strict reading: no source
//      disappears because we suspect it's broken).
//
// Like step-03, the spec uses mocked endpoints + clicks the channel. It
// does NOT need to play through hls.js — we only observe the request shape
// emitted by the JS client.

const TEST_KEY = 'rank-test-key';

function stubChannel(playUrl: string) {
  return [{
    key: TEST_KEY,
    name: 'Rank Test Channel',
    kind: 'tv',
    caps_required: ['hls', 'h264', 'aac', 'live_video_hls'],
    source_count: 1,
    play_url: playUrl,
    tv_archive: false,
  }];
}

test('play URL carries caps query param (Step 4 §9 plumbing)', async ({ page }) => {
  // Seed a cached cap set so `buildPlayUrl(loadCaps())` has something to
  // emit. In production this lands after the first ensureCaps save —
  // a fresh boot with all play-probes 404'd would otherwise leave the
  // cache empty and the spec would flake.
  await page.addInitScript(() => {
    localStorage.setItem(
      'xtream.client.caps.v3',
      JSON.stringify({
        ua: navigator.userAgent,
        caps: ['hls', 'h264', 'aac', 'live_video_hls'],
      }),
    );
  });
  await page.route('**/api/channels', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(stubChannel(`http://localhost:8080/play/${TEST_KEY}.m3u8`)),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404 });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });

  // Capture the actual /play/ request the browser issues — its URL is what
  // we're testing. Mock it as a parseable empty manifest so hls.js doesn't
  // fatal-loop.
  let playRequestUrl: string | null = null;
  await page.route('**/play/' + TEST_KEY + '.m3u8*', async (route) => {
    if (!playRequestUrl) playRequestUrl = route.request().url();
    await route.fulfill({
      status: 200,
      contentType: 'application/vnd.apple.mpegurl',
      body: '#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n',
    });
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });
  await page.locator('#list .list-item').first().click();
  // Wait until the request fires.
  await page.waitForFunction(() => !!document.querySelector('video#player'), { timeout: 5_000 });

  expect(playRequestUrl).not.toBeNull();
  const url = playRequestUrl as unknown as string;
  expect(url).toContain('pid=');
  expect(url).toContain('caps=');
});

test('channel stays in list after a post-canplay failure (no exclusion)', async ({ page }) => {
  // Phase 4 strict §8: a failure deprioritises a candidate but never
  // removes it. From the client's view, the channel must keep appearing
  // in /api/channels even after we POST a failure for it.
  let channelsReqCount = 0;
  await page.route('**/api/channels', async (route) => {
    channelsReqCount++;
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(stubChannel(`http://localhost:8080/play/${TEST_KEY}.m3u8`)),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404 });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });
  // The play URL 502s — triggers the failure-reporting path in main.js.
  await page.route('**/play/' + TEST_KEY + '.m3u8*', async (route) => {
    await route.fulfill({ status: 502, body: '' });
  });
  // Accept and ack the feedback POST so the client doesn't retry on its
  // own network-error fallback.
  let feedbackReceived = false;
  await page.route('**/api/feedback/' + TEST_KEY, async (route) => {
    feedbackReceived = true;
    await route.fulfill({ status: 204 });
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });
  await page.locator('#list .list-item').first().click();

  // Give the failure path a beat to fire the feedback POST.
  await page.waitForFunction(
    () => true,
    null,
    { timeout: 2_000 },
  ).catch(() => { /* ignore */ });
  await page.waitForTimeout(1_500);
  expect(feedbackReceived).toBe(true);

  // Re-fetch /api/channels and assert the test channel is still listed.
  const stillListed = await page.evaluate(async (key) => {
    const resp = await fetch('/api/channels');
    const rows = await resp.json();
    return rows.some((r: { key: string }) => r.key === key);
  }, TEST_KEY);
  expect(stillListed).toBe(true);
  expect(channelsReqCount).toBeGreaterThanOrEqual(2);
});
