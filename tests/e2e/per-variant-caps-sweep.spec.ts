import { test, expect, Page, Browser, chromium } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';

// End-to-end validation sweep for the per-variant `caps_required` design.
//
// Driven against a locally-launched iptv-proxy. Two runs per invocation
// (controlled by env vars):
//   - baseline (caps_v2_per_variant=false)   — point at proxy on :8081
//   - post-cutover (caps_v2_per_variant=true) — point at proxy on :8082
//
// Outputs: test-results/per-variant-caps-sweep-<flag>-<UA>-<ts>.json
//
// Pass criteria per plan: readyState >= 2, buffered.length > 0 with
// buffered.end(0) >= 1, currentTime advances over a 2 s observation.

type ChannelInfo = {
  key: string;
  name: string;
  kind: 'tv' | 'radio';
  caps_required: string[];
  play_url: string;
  format?: string;
};

type CandidateOutcome = {
  stream_id: number;
  host: string;
  url: string;
  outcome: 'pass' | 'fail';
  error?: string;
};

type ChannelResult = {
  channel: string;
  ua: string;
  served_outcome: 'pass' | 'fail';
  served_error?: string;
  candidates_tested?: CandidateOutcome[];
  any_playable?: boolean;
  missed_opportunity?: boolean;
};

type AggregateReport = {
  base_url: string;
  ua_profile: string;
  seed: number;
  flag_state: string;
  total_channels_tested: number;
  failed_channels: number;
  failed_with_no_playable_candidate: number;
  missed_opportunities: number;
  per_channel: ChannelResult[];
  generated_at: string;
};

const PROXY_BASE = process.env.IPTV_PROXY_BASE || 'http://127.0.0.1:8081';
const FLAG_STATE = process.env.IPTV_FLAG_STATE || 'baseline';
const SEED = parseInt(process.env.IPTV_SEED || '1234', 10);
const N_RANDOM = parseInt(process.env.IPTV_SWEEP_N || '10', 10);
const PLAY_WINDOW_MS = parseInt(process.env.IPTV_PLAY_WINDOW_MS || '6000', 10);
// R1 round-1: bumped from 3 to 12. Channels like TVI carry ~48 candidates
// (multi-variant × 8 hosts); capping at 3 means real misses at rank 4+
// get classified as `failed_with_no_playable_candidate` and never surface
// as `missed_opportunity`. 12 covers the rank-winner variant across every
// alive host plus a sibling variant on its top host — enough to catch
// the rank-winner-vs-floor cap-eviction concern the plan calls out
// without making the sweep wall-clock unbounded. Override with
// `IPTV_MAX_CANDIDATES=0` to test every candidate (no cap).
const _RAW_MAX_CANDIDATES = parseInt(process.env.IPTV_MAX_CANDIDATES || '12', 10);
const MAX_CANDIDATES =
  _RAW_MAX_CANDIDATES === 0 ? Number.MAX_SAFE_INTEGER : _RAW_MAX_CANDIDATES;
const TARGETED_KEYS = (process.env.IPTV_TARGETED || 'rtp1,rtp2,sic,tvi').split(',');
// Caps the simulated Chromium client advertises. webOS profile adds
// h264_excess_refs (matching the design's expectation that webOS tolerates
// it). For baseline (flag=false) caps don't gate; for post-cutover, the
// /api/channels filter actually uses these.
const CHROMIUM_CAPS = 'hls,h264,aac,live_video_hls,live_audio_only_hls,hevc,mse,hls_mse';
const WEBOS_CAPS = CHROMIUM_CAPS + ',h264_excess_refs,hls_native';

// Write to a dir Playwright won't auto-clean between runs (it nukes
// `test-results/` per spec). Falling back to test-results/ for the
// per-spec artifact dir; report JSONs go to IPTV_RESULTS_DIR if set.
const RESULTS_DIR =
  process.env.IPTV_RESULTS_DIR || path.resolve(__dirname, '../../test-results');

function seedRng(seed: number) {
  let s = seed >>> 0;
  return function () {
    s = (s + 0x6D2B79F5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

async function fetchJson<T>(url: string, caps?: string): Promise<T> {
  const headers: Record<string, string> = {};
  if (caps) headers['X-Client-Caps'] = caps;
  const r = await fetch(url, { headers });
  if (!r.ok) throw new Error(`HTTP ${r.status} on ${url}`);
  return (await r.json()) as T;
}

function sampleN<T>(arr: T[], n: number, rng: () => number): T[] {
  const copy = arr.slice();
  const out: T[] = [];
  while (out.length < n && copy.length) {
    const i = Math.floor(rng() * copy.length);
    out.push(copy.splice(i, 1)[0]);
  }
  return out;
}

function b64UrlNoPad(s: string) {
  return Buffer.from(s, 'utf-8')
    .toString('base64')
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/g, '');
}

async function tryPlayChannel(
  page: Page,
  playUrl: string,
  windowMs: number,
): Promise<{ ok: boolean; reason?: string }> {
  return await page.evaluate(
    async ({ url, windowMs }) => {
      const v = document.createElement('video');
      v.muted = true;
      v.preload = 'auto';
      v.style.display = 'none';
      document.body.appendChild(v);
      const Hls = (window as any).Hls;
      let hls: any = null;
      const cleanup = () => {
        try { if (hls) hls.destroy(); } catch (e) {}
        try { v.pause(); v.removeAttribute('src'); v.load(); v.remove(); } catch (e) {}
      };
      try {
        if (Hls && Hls.isSupported && Hls.isSupported()) {
          hls = new Hls({ manifestLoadingTimeOut: 5000, fragLoadingTimeOut: 5000 });
          hls.loadSource(url);
          hls.attachMedia(v);
        } else {
          v.src = url;
          v.load();
        }
        const t0 = Date.now();
        let ok = false;
        while (Date.now() - t0 < windowMs) {
          await new Promise((r) => setTimeout(r, 200));
          if (v.readyState >= 2 && v.buffered.length > 0 && v.buffered.end(0) >= 1) {
            ok = true;
            break;
          }
        }
        if (!ok) {
          cleanup();
          return { ok: false, reason: 'no-buffered-frame' };
        }
        const t1 = v.currentTime;
        try { await v.play().catch(() => {}); } catch (e) {}
        await new Promise((r) => setTimeout(r, 1500));
        const advanced = v.currentTime > t1;
        cleanup();
        if (!advanced) return { ok: false, reason: 'currentTime-stuck' };
        return { ok: true };
      } catch (err) {
        cleanup();
        return { ok: false, reason: 'exception:' + String(err) };
      }
    },
    { url: playUrl, windowMs },
  );
}

type CandidateDto = {
  url: string;
  host: string;
  stream_id: number;
  rank_pos: number;
};

async function fetchCandidates(channelKey: string): Promise<CandidateDto[]> {
  try {
    return await fetchJson<CandidateDto[]>(
      `${PROXY_BASE}/api/candidates/${encodeURIComponent(channelKey)}`,
    );
  } catch (e) {
    return [];
  }
}

async function testCandidate(
  page: Page,
  channel: ChannelInfo,
  cand: CandidateDto,
  caps: string,
): Promise<CandidateOutcome> {
  const force = b64UrlNoPad(cand.url);
  const playUrl =
    channel.play_url +
    (channel.play_url.indexOf('?') >= 0 ? '&' : '?') +
    'caps=' + encodeURIComponent(caps) +
    '&force_url=' + encodeURIComponent(force);
  const res = await tryPlayChannel(page, playUrl, PLAY_WINDOW_MS);
  return {
    stream_id: cand.stream_id,
    host: cand.host,
    url: cand.url,
    outcome: res.ok ? 'pass' : 'fail',
    error: res.reason,
  };
}

async function runForProfile(browser: Browser, uaProfile: string): Promise<AggregateReport> {
  const rng = seedRng(SEED);
  const caps = uaProfile === 'webOS' ? WEBOS_CAPS : CHROMIUM_CAPS;
  const ctx = await browser.newContext({
    userAgent:
      uaProfile === 'webOS'
        ? 'Mozilla/5.0 (Web0S; Linux/SmartTV) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/87.0.4280.88 Safari/537.36 WebAppManager'
        : undefined,
  });
  const page = await ctx.newPage();
  await page.goto(PROXY_BASE);

  const allChannels = await fetchJson<ChannelInfo[]>(`${PROXY_BASE}/api/channels`, caps);
  const tvOnly = allChannels.filter((c) => c.kind === 'tv');
  const random = sampleN(tvOnly, N_RANDOM, rng);
  const targeted = TARGETED_KEYS.map((k) => allChannels.find((c) => c.key === k)).filter(
    (x): x is ChannelInfo => !!x,
  );
  const seen = new Set<string>();
  const picks: ChannelInfo[] = [];
  for (const c of [...targeted, ...random]) {
    if (seen.has(c.key)) continue;
    seen.add(c.key);
    picks.push(c);
  }

  const perChannel: ChannelResult[] = [];
  for (const ch of picks) {
    const result: ChannelResult = {
      channel: ch.key,
      ua: uaProfile,
      served_outcome: 'fail',
    };
    const playUrl =
      ch.play_url +
      (ch.play_url.indexOf('?') >= 0 ? '&' : '?') +
      'caps=' + encodeURIComponent(caps);
    const res = await tryPlayChannel(page, playUrl, PLAY_WINDOW_MS);
    result.served_outcome = res.ok ? 'pass' : 'fail';
    result.served_error = res.reason;
    if (!res.ok) {
      const cands = await fetchCandidates(ch.key);
      const tested: CandidateOutcome[] = [];
      let anyPlayable = false;
      for (const cand of cands.slice(0, MAX_CANDIDATES)) {
        const out = await testCandidate(page, ch, cand, caps);
        if (out.outcome === 'pass') anyPlayable = true;
        tested.push(out);
        if (anyPlayable) break;
      }
      result.candidates_tested = tested;
      result.any_playable = anyPlayable;
      result.missed_opportunity = anyPlayable;
    }
    perChannel.push(result);
  }

  await ctx.close();
  const failed = perChannel.filter((r) => r.served_outcome === 'fail');
  const failedNoCandidate = failed.filter((r) => !r.any_playable).length;
  const missed = failed.filter((r) => r.missed_opportunity).length;
  return {
    base_url: PROXY_BASE,
    ua_profile: uaProfile,
    seed: SEED,
    flag_state: FLAG_STATE,
    total_channels_tested: perChannel.length,
    failed_channels: failed.length,
    failed_with_no_playable_candidate: failedNoCandidate,
    missed_opportunities: missed,
    per_channel: perChannel,
    generated_at: new Date().toISOString(),
  };
}

function writeReport(report: AggregateReport, ua: string) {
  const ts = new Date().toISOString().replace(/[:.]/g, '-');
  fs.mkdirSync(RESULTS_DIR, { recursive: true });
  const p = path.join(
    RESULTS_DIR,
    `per-variant-caps-sweep-${FLAG_STATE}-${ua}-${ts}.json`,
  );
  fs.writeFileSync(p, JSON.stringify(report, null, 2));
  console.log(
    `WROTE ${p}: total=${report.total_channels_tested} failed=${report.failed_channels} missed=${report.missed_opportunities}`,
  );
}

test.describe('per-variant caps E2E sweep', () => {
  test.skip(
    !process.env.IPTV_SWEEP,
    'set IPTV_SWEEP=1 to run the per-variant-caps E2E sweep against a local proxy',
  );

  test('Chromium UA sweep', async () => {
    test.setTimeout(10 * 60 * 1000);
    const browser = await chromium.launch();
    try {
      const report = await runForProfile(browser, 'Chromium');
      writeReport(report, 'Chromium');
      expect(report.total_channels_tested).toBeGreaterThan(0);
    } finally {
      await browser.close();
    }
  });

  // webOS sweep intentionally omitted — operator runs the actual webOS TV
  // for personal use, no UA-shim variants in this suite.
});
