import { test, expect } from '@playwright/test';
import { useLocalConfig } from './_helpers';

test.beforeEach(async ({ page }) => { await useLocalConfig(page); });

// Plan: docs/plan-source-reliability.md §10 (radio ADTS extraction).
//
// What this spec pins:
//   The `/admin/measured-quality` JSON surface exposes `sample_rate_hz`
//   and `audio_channels` on radio entries — the two new fields Phase 8
//   added to `Sample` and `MeasuredQuality`. We inject a synthetic ADTS-
//   derived Sample via the IPTV_TEST_HOOKS endpoint (same pattern as
//   step-06) rather than running a real radio fixture through hls.js +
//   the proxy_segment audio branch, which would need a committed binary
//   AAC fixture and a docker-compose harness this repo doesn't ship.
//
// The Rust unit tests in `adts.rs` cover the parser; in `proxy.rs` cover
// the radio rank tuple; in `measured.rs` cover the on-disk round-trip.
// This spec proves the wire shape from the client's perspective.

test('radio sample exposes sample_rate_hz + audio_channels in measured-quality', async ({ page }) => {
  const stream_id = 0x8000_0000_0001; // synthetic radio-side stream_id (high-bit set, matches canonical.rs)
  const host = 'http://step10-radio-host';

  const inject = await page.request.post('/admin/inject-sample', {
    data: {
      stream_id,
      host,
      sample: {
        at: '2026-05-17T14:00:00Z',
        source: 'Sweep',
        width: 0,
        height: 0,
        codec: 'aac',
        // Phase 8 audio fields — equivalent to what classify_aac_chunk
        // produces from a real AAC LC stereo 44.1 kHz segment.
        bitrate_kbps: 128,
        sample_rate_hz: 44100,
        audio_channels: 2,
      },
    },
  });
  expect(inject.ok()).toBeTruthy();

  const mq = await page.request.get('/admin/measured-quality');
  expect(mq.ok()).toBeTruthy();
  const entries = await mq.json();
  const ours = entries.find((e: { stream_id: number; host: string }) =>
    e.stream_id === stream_id && e.host === host);
  expect(ours).toBeDefined();
  expect(ours.aggregate.codec).toBe('aac');
  expect(ours.aggregate.sample_rate_hz).toBe(44100);
  expect(ours.aggregate.audio_channels).toBe(2);
  expect(ours.aggregate.bitrate_kbps).toBe(128);
});
