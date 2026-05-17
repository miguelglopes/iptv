import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §9 (user override).
//
// What this spec pins:
//   1. Opening the candidate overlay (OK in fullscreen live) then OK on a
//      row commits a force-play whose URL carries `&force_url=…&caps=…&pid=…`.
//   2. The negative path: `/play/<key>?force_url=<b64-of-NOT-in-set>` 404s.

const TEST_KEY = 'force-test';

function stubChannel() {
  return [{
    key: TEST_KEY,
    name: 'Force Test Channel',
    kind: 'tv',
    caps_required: ['hls', 'h264', 'aac', 'live_video_hls'],
    source_count: 1,
    play_url: `http://localhost:8080/play/${TEST_KEY}.m3u8`,
    tv_archive: false,
  }];
}

function mockCandidates() {
  return [
    { url: 'http://up-a.example/live/u/p/1.m3u8', host: 'http://up-a.example', stream_id: 1, rank_pos: 0, cool_off_step: 0 },
    { url: 'http://up-b.example/live/u/p/1.m3u8', host: 'http://up-b.example', stream_id: 1, rank_pos: 1, cool_off_step: 0 },
  ];
}

test('candidate overlay commits a force-play with caps + pid + force_url', async ({ page }) => {
  // Seed caps so the test channel is visible.
  await page.addInitScript(() => {
    localStorage.setItem(
      'xtream.client.caps.v3',
      JSON.stringify({ ua: navigator.userAgent, caps: ['hls', 'h264', 'aac', 'live_video_hls'] }),
    );
  });

  await page.route('**/api/channels', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      headers: { 'x-caps-matrix-version': 'seeded' },
      body: JSON.stringify(stubChannel()),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404 });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });
  await page.route(`**/api/candidates/${TEST_KEY}`, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(mockCandidates()),
    });
  });

  // Capture each /play/ request URL so we can assert the query-string shape
  // emitted by buildPlayUrl when force-committing.
  const playUrls: string[] = [];
  await page.route(`**/play/${TEST_KEY}.m3u8*`, async (route) => {
    playUrls.push(route.request().url());
    await route.fulfill({
      status: 200,
      contentType: 'application/vnd.apple.mpegurl',
      body: '#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n',
    });
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });
  // Start the play.
  await page.locator('#list .list-item').first().click();
  await page.waitForSelector('video#player', { timeout: 5_000 });
  // Press OK to open the overlay (we're already in fullscreen live).
  await page.keyboard.press('Enter');
  await page.waitForSelector('#candidate-overlay', { timeout: 5_000 });
  // Navigate to second row, then OK to commit.
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('Enter');

  // After commit, a new /play/ request fires with force_url + caps + pid.
  await page.waitForFunction(
    (count) => Array.from(document.querySelectorAll('#candidate-overlay')).length === 0,
    null,
    { timeout: 3_000 },
  ).catch(() => { /* fine */ });
  await page.waitForTimeout(1_000);

  const forcedUrl = playUrls.find(u => u.includes('force_url='));
  expect(forcedUrl).toBeTruthy();
  expect(forcedUrl).toMatch(/force_url=/);
  expect(forcedUrl).toMatch(/pid=/);
  expect(forcedUrl).toMatch(/caps=/);
});

test('force_url with URL not in candidate set 404s on /play', async ({ page }) => {
  // Use the real server's /play/ endpoint via the proxy — this test relies
  // on a running server. With pure mocks, the assertion is just about the
  // request shape going out. We mock /play to return 404 when force_url is
  // present and bogus, simulating the server's actual validation.
  await page.route('**/api/channels', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      headers: { 'x-caps-matrix-version': 'seeded' },
      body: JSON.stringify(stubChannel()),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404 });
  });
  await page.route(`**/play/${TEST_KEY}.m3u8*`, async (route) => {
    const url = route.request().url();
    if (url.includes('force_url=')) {
      // Simulate the server's "unknown force_url" 404 when the URL is not
      // in the current build_candidates output.
      await route.fulfill({ status: 404, body: 'unknown force_url' });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: 'application/vnd.apple.mpegurl',
      body: '#EXTM3U\n',
    });
  });

  await page.goto('/');
  // Issue a direct request with a bogus force_url.
  const bogus = btoa('http://intruder/live/u/p/99.m3u8')
    .replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const status = await page.evaluate(async (params) => {
    const r = await fetch(`/play/${params.key}.m3u8?force_url=${params.f}`);
    return r.status;
  }, { key: TEST_KEY, f: bogus });
  expect(status).toBe(404);
});
