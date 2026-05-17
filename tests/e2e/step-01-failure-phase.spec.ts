import { test, expect, type Request } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §1 (Failure-phase signal).
// What this pins: the wire shape. When player.js sees the manifest fail
// before readyState >= 2, the resulting POST /api/feedback/:key body
// carries `phase: "pre-canplay"`. State-machine consumption of `phase`
// lands in a later plan step; this spec only proves the signal travels.
//
// Assumes a running `docker compose up` (or `cargo run`) of the proxy on
// :8080 — `npm test` doesn't start one. The spec mocks /api/channels and
// the /play/ manifest so it never touches a real upstream.

const TEST_KEY = 'phase-test-key';

async function readJsonBody(req: Request): Promise<unknown> {
  const raw = req.postData();
  if (!raw) return null;
  try { return JSON.parse(raw); } catch { return raw; }
}

test('pre-canplay manifest 502 → feedback POST carries phase=pre-canplay', async ({ page }) => {
  // Stub the catalog so we know the channel key the play URL targets.
  await page.route('**/api/channels', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([
        {
          key: TEST_KEY,
          name: 'Phase Test Channel',
          kind: 'tv',
          caps_required: ['hls', 'h264', 'aac', 'live_video_hls'],
          source_count: 1,
          play_url: `http://localhost:8080/play/${TEST_KEY}.m3u8`,
          tv_archive: false,
        },
      ]),
    });
  });

  // Cap probes redirect to a play URL. Short-circuit them so the boot
  // sequence completes without touching real channels.
  await page.route('**/api/probe/**', async (route) => {
    await route.fulfill({ status: 404, body: 'not probed in this test' });
  });

  // Stub EPG (channel may request it for the focused row).
  await page.route('**/api/epg/**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: '[]',
    });
  });

  // The manifest fetch — return 502 so the player never reaches readyState>=2.
  // hls.js (laptop bundle) fires a fatal MANIFEST_LOAD_ERROR on this; webOS
  // native path fires the <video> error event. Either way, phase must be
  // "pre-canplay" because we never got a frame.
  await page.route('**/play/' + TEST_KEY + '.m3u8*', async (route) => {
    await route.fulfill({ status: 502, body: '' });
  });

  // Capture the feedback POST before navigating, so we don't race the page.
  const feedbackReq = page.waitForRequest(
    (req) => req.url().includes('/api/feedback/' + TEST_KEY) && req.method() === 'POST',
    { timeout: 20_000 },
  );

  await page.goto('/');

  // Wait for the channel list to render at least one item. listHtml in
  // app/js/main.js writes `.list-item` divs with `data-i` per row; only
  // navigable rows have that class (section headers are excluded).
  await page.waitForSelector('#list .list-item', { timeout: 10_000 });

  // Mocked /api/channels returned exactly our test channel, so the first
  // .list-item is it. Clicking calls activate() → play() → mocked 502.
  await page.locator('#list .list-item').first().click();

  const req = await feedbackReq;
  const body = await readJsonBody(req) as Record<string, unknown> | null;
  expect(body).not.toBeNull();
  expect(body!.kind).toBe('fail');
  expect(body!.phase).toBe('pre-canplay');
  expect(typeof body!.play_id).toBe('string');

  // TODO (later phase): construct a reliable post-canplay scenario — needs
  // the manifest to succeed plus segment-time errors. Requires fixture
  // segments and a media-decoder Playwright can actually play (H.264/AAC
  // on chromium). Deferred until a later step where we already need that
  // fixture for §5 / §6 testing.
});
