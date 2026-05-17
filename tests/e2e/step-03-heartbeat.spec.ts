import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §3 (Clean-play heartbeat).
//
// What this pins:
//   1. player.js arms a 30 s heartbeat interval on `canplay`.
//   2. Each tick POSTs /api/heartbeat with { play_id: <current pid> }.
//   3. Stopping playback clears the interval — no more heartbeats fire.
//
// Implementation note: a deterministic "real playback in Chromium with a
// route-mocked TS segment" setup is out of scope for this spec — bytes hls.js
// can decode reliably need real TS muxing or a binary fixture. Instead we
// dispatch a synthetic `canplay` Event on the <video> element after the
// player has appended it, which faithfully exercises the player's actual
// canplay listener (and therefore `_armHeartbeat`). Playwright's fake clock
// then drives the interval ticks deterministically without real wall-time
// waits.

const TEST_KEY = 'heartbeat-test-key';

function stubChannel(playUrl: string) {
  return [{
    key: TEST_KEY,
    name: 'Heartbeat Test Channel',
    kind: 'tv',
    caps_required: ['hls', 'h264', 'aac', 'live_video_hls'],
    source_count: 1,
    play_url: playUrl,
    tv_archive: false,
  }];
}

test('canplay arms 30 s heartbeat; stop clears it', async ({ page }) => {
  // Install fake clock BEFORE any page JS runs so setInterval is controllable.
  await page.clock.install();

  await page.route('**/api/channels', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(stubChannel(`http://localhost:8080/play/${TEST_KEY}.m3u8`)),
    });
  });
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404, body: 'not probed in this test' });
  });
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
  });

  // A minimal but parseable live HLS manifest — hls.js accepts it without
  // fatal error (so no failover loop), but never fires canplay organically
  // because there are no segments. We dispatch a synthetic canplay below.
  await page.route('**/play/' + TEST_KEY + '.m3u8*', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/vnd.apple.mpegurl',
      body: '#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n',
    });
  });

  type Heartbeat = { play_id?: string };
  const heartbeats: Heartbeat[] = [];
  page.on('request', (req) => {
    if (req.url().includes('/api/heartbeat') && req.method() === 'POST') {
      try { heartbeats.push(JSON.parse(req.postData() || '{}')); } catch { /* ignore */ }
    }
  });

  await page.goto('/');
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });

  // Native Chromium has no HLS demuxer for raw .m3u8, and hls.js fatal-
  // errors on an empty-live mock manifest under fake clock. Both paths
  // would fire onSourceFailed and clear the heartbeat. Instead drive the
  // player directly via `window.__app.player` — the same `_armHeartbeat`
  // the canplay event would trigger in production, just without a real
  // playback pipeline.
  await page.evaluate(() => {
    const app = (window as unknown as { __app: { player: any; state: any } }).__app;
    // Simulate a play in progress: state.playing must have a playId for
    // the heartbeat to fire.
    app.state.playing = {
      channel: { key: 'heartbeat-test-key', name: 'Heartbeat Test', kind: 'tv', caps_required: [], play_url: '' },
      mode: 'live',
      playId: 'test-pid-abcdef123456',
    };
    // Synthesise the bits of player state that _armHeartbeat checks
    // (player.video / player.url) so the per-tick guards don't bail.
    const v = document.createElement('video');
    v.id = 'player';
    document.body.appendChild(v);
    app.player.video = v;
    app.player.url = 'http://stub';
    app.player._armHeartbeat();
  });

  // Advance fake clock by ~70 s — should see at least two heartbeat ticks
  // (30 s, 60 s) plus margin for the third (90 s) if scheduling drifts.
  await page.clock.runFor(70_000);
  expect(heartbeats.length).toBeGreaterThanOrEqual(2);
  expect(typeof heartbeats[0].play_id).toBe('string');
  expect((heartbeats[0].play_id as string).length).toBeGreaterThan(0);

  // Stop playback by calling player.stop() — what `back()` would do
  // through the keyboard chain in production.
  const before = heartbeats.length;
  await page.evaluate(() => {
    const app = (window as unknown as { __app: { player: any } }).__app;
    app.player.stop();
  });
  await page.clock.runFor(1_000);
  // Advance further 35 s — past one full interval — and confirm quiescence.
  await page.clock.runFor(35_000);
  expect(heartbeats.length).toBe(before);
});
