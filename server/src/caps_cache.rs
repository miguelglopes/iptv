// Per-channel `caps_required` cache + cap-matrix versioning (plan §7).
//
// The cache is keyed by channel.key and holds the tightened cap list for
// each channel. Invalidation: rebuild when either the catalog or the
// measured-quality store has moved since the last build. Catalog freshness
// is tracked via `CatalogSnapshot::last_refreshed`; measured freshness via
// `MeasuredStore::generation()` (a monotonic counter that's independent of
// the disk-flush `dirty` bool so the two consumers don't race).
//
// The "matrix version" header (`X-Caps-Matrix-Version`) is a stable digest
// of the cap matrix surface that the client cares about: the set of
// (probe_endpoint → picked_channel_key) tuples plus the catalog size. When
// the version flips, the client clears its local cap cache and re-probes
// before the next `/api/channels` request — without this the freshness
// loop could tighten server-side caps while the client still believes it
// has the looser set, silently hiding channels.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use parking_lot::RwLock;
use time::OffsetDateTime;

use crate::canonical::CanonicalChannel;
use crate::catalog::CatalogSnapshot;
use crate::default_order::Curation;
use crate::measured::MeasuredStore;
use crate::radio::RadioFormat;
use crate::xtream::ChannelKind;

/// The five tightening probes the client matrix exposes. Order matters: it
/// fixes the iteration order used by `compute_version` so the same world
/// always hashes to the same string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeTarget {
    H264,
    Hevc,
    HevcMain10,
    Av1,
    DvbSafe,
}

impl ProbeTarget {
    pub const ALL: &'static [ProbeTarget] = &[
        ProbeTarget::H264,
        ProbeTarget::Hevc,
        ProbeTarget::HevcMain10,
        ProbeTarget::Av1,
        ProbeTarget::DvbSafe,
    ];

    pub fn endpoint_name(self) -> &'static str {
        match self {
            ProbeTarget::H264 => "h264",
            ProbeTarget::Hevc => "hevc",
            ProbeTarget::HevcMain10 => "hevc_main10",
            ProbeTarget::Av1 => "av1",
            ProbeTarget::DvbSafe => "dvb_safe",
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2/3/4: per-variant cap derivation. Active behind
// `Config::caps_v2_per_variant`. The legacy `caps_required` API below stays
// unchanged for the default-off case so existing tests don't drift.

/// Cap tags this variant requires on a given host. Pulls the aggregate
/// `caps_required` straight from the measured store; returns `None` when
/// no sample exists (caller treats absent host as "unknown — no vote").
pub fn caps_required_at_host(
    measured: &crate::measured::MeasuredStore,
    stream_id: u64,
    host: &str,
) -> Option<Vec<String>> {
    measured
        .get(stream_id, host)
        .map(|q| q.caps_required.clone())
}

/// True iff the (variant, host) pair has had a successful sample within
/// `stale_secs`. Used by the v2 "stale variant escape": variants
/// unreachable on every alive host for too long are dropped from
/// `build_candidates`.
pub fn host_is_fresh(
    measured: &crate::measured::MeasuredStore,
    stream_id: u64,
    host: &str,
    stale_secs: u64,
    now: time::OffsetDateTime,
) -> bool {
    let Some(at) = measured.most_recent_at(stream_id, host) else {
        return false;
    };
    if stale_secs == 0 {
        return true;
    }
    (now - at).whole_seconds().max(0) as u64 <= stale_secs
}

/// Cross-host union of `caps_required` for one variant within the v2
/// emit scope: `alive ∧ ¬blacklisted ∧ ¬stale`. Returns `None` when
/// the variant is stale on every alive host (caller drops it).
pub fn variant_caps_required(
    measured: &crate::measured::MeasuredStore,
    blacklist: &crate::blacklist::Blacklist,
    stream_id: u64,
    alive_hosts: &[String],
    stale_secs: u64,
    now: time::OffsetDateTime,
) -> Option<Vec<String>> {
    let mut any_fresh = false;
    let mut union: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for host in alive_hosts {
        if blacklist.is_host_bad(host) {
            continue;
        }
        if !host_is_fresh(measured, stream_id, host, stale_secs, now) {
            continue;
        }
        any_fresh = true;
        if let Some(caps) = caps_required_at_host(measured, stream_id, host) {
            for c in caps {
                union.insert(c);
            }
        }
    }
    if !any_fresh {
        return None;
    }
    Some(union.into_iter().collect())
}

/// Compute the tightened cap list for one channel, given fresh inputs.
///
/// Algorithm (plan §7 lines 297-303):
///   1. Per-kind baseline.
///   2. Collect MeasuredQuality across the channel's (source × alive_host)
///      pairs.
///   3. Sparse data (no samples) → keep baseline.
///   4. All HEVC → swap `h264` for `hevc`.
///   5. All HEVC + 10-bit → add `hevc_main10`.
///   6. All `dvb_unsafe = true` → add `dvb_safe`.
///
/// Radio keeps its existing per-format derivation untouched — Step 7's
/// tightening rules are TV-only.
pub fn caps_required(
    channel: &CanonicalChannel,
    measured: &MeasuredStore,
    alive_hosts: &[String],
) -> Vec<&'static str> {
    // Radio: derive from the channel's source formats. Unchanged from the
    // pre-Phase-6 logic — radio doesn't have HEVC/main10/dvb_unsafe.
    if let ChannelKind::Radio = channel.kind {
        let mut fmts: std::collections::HashSet<RadioFormat> = channel
            .sources
            .iter()
            .filter_map(|s| s.radio_format)
            .collect();
        if fmts.is_empty() {
            fmts.insert(RadioFormat::Hls);
        }
        return radio_caps_for(&fmts);
    }

    // TV baseline.
    let mut caps: Vec<&'static str> = vec!["hls", "h264", "aac", "live_video_hls"];

    // Gather aggregates for each (source × alive_host) that has samples.
    // Also peek at `origin_host` even if it's not currently alive — the
    // freshness loop may have stale samples we still want to consult.
    let mut measured_quality = Vec::new();
    for src in &channel.sources {
        if src.direct_source.is_some() {
            continue;
        }
        for host in alive_hosts {
            if let Some(q) = measured.get(src.stream_id, host) {
                measured_quality.push(q);
            }
        }
        if !src.origin_host.is_empty()
            && !alive_hosts.iter().any(|h| h == &src.origin_host)
        {
            if let Some(q) = measured.get(src.stream_id, &src.origin_host) {
                measured_quality.push(q);
            }
        }
    }

    // Sparse data → don't tighten. Hiding a channel based on zero samples
    // would falsely punish channels the freshness loop hasn't reached yet.
    if measured_quality.is_empty() {
        return caps;
    }

    let all_hevc = measured_quality
        .iter()
        .all(|q| q.codec.as_deref() == Some("hevc"));
    if all_hevc {
        for c in caps.iter_mut() {
            if *c == "h264" {
                *c = "hevc";
            }
        }
    }

    let all_main10 = measured_quality.iter().all(|q| {
        q.codec.as_deref() == Some("hevc")
            && q.pix_fmt.as_deref().map(|p| p.contains("10")).unwrap_or(false)
    });
    if all_main10 {
        caps.push("hevc_main10");
    }

    let all_dvb_unsafe = measured_quality.iter().all(|q| q.dvb_unsafe == Some(true));
    if all_dvb_unsafe {
        caps.push("dvb_safe");
    }

    caps
}

fn radio_caps_for(fmts: &std::collections::HashSet<RadioFormat>) -> Vec<&'static str> {
    if fmts.contains(&RadioFormat::Hls) {
        return vec!["hls", "aac", "live_audio_only_hls"];
    }
    vec!["aac"]
}

/// True if the channel is uniformly classified per the probe target — i.e.
/// every measured (stream_id × alive_host) sample for this channel agrees.
/// Sparse data → false (we don't speculate). Used both by the per-channel
/// `caps_required` algorithm (indirectly) and by `pick_probe_channel` to
/// find a probe target.
pub fn channel_matches_probe(
    channel: &CanonicalChannel,
    measured: &MeasuredStore,
    alive_hosts: &[String],
    target: ProbeTarget,
) -> bool {
    if channel.kind == ChannelKind::Radio {
        return false;
    }
    let mut samples = Vec::new();
    for src in &channel.sources {
        if src.direct_source.is_some() {
            continue;
        }
        for host in alive_hosts {
            if let Some(q) = measured.get(src.stream_id, host) {
                samples.push(q);
            }
        }
        if !src.origin_host.is_empty()
            && !alive_hosts.iter().any(|h| h == &src.origin_host)
        {
            if let Some(q) = measured.get(src.stream_id, &src.origin_host) {
                samples.push(q);
            }
        }
    }
    if samples.is_empty() {
        return false;
    }
    samples.iter().all(|q| match target {
        ProbeTarget::H264 => q.codec.as_deref() == Some("h264"),
        ProbeTarget::Hevc => q.codec.as_deref() == Some("hevc"),
        ProbeTarget::HevcMain10 => {
            q.codec.as_deref() == Some("hevc")
                && q.pix_fmt.as_deref().map(|p| p.contains("10")).unwrap_or(false)
        }
        ProbeTarget::Av1 => q.codec.as_deref() == Some("av1"),
        ProbeTarget::DvbSafe => q.dvb_unsafe == Some(true),
    })
}

/// Pick the highest-curation-ranked channel matching `target`, or `None`
/// if no channel matches.
pub fn pick_probe_channel<'a>(
    snap: &'a CatalogSnapshot,
    curation: &Curation,
    measured: &MeasuredStore,
    alive_hosts: &[String],
    target: ProbeTarget,
) -> Option<&'a CanonicalChannel> {
    snap.channels
        .iter()
        .filter(|c| channel_matches_probe(c, measured, alive_hosts, target))
        .min_by_key(|c| curation.rank_of(&c.key).unwrap_or(usize::MAX))
}

/// Phase 3 helper: pick a channel + variant whose `caps_required` carries
/// `target_cap`, ranked by curation. Used by the JSON probe endpoint to
/// drive the conditional probe (e.g. `h264_excess_refs`). Returns the
/// (channel key, upstream stream_id) tuple — the play loop will pin to
/// that stream_id when the client visits `/play/<key>?probe=1&probe_stream_id=<sid>`.
pub fn pick_probe_variant(
    snap: &CatalogSnapshot,
    curation: &Curation,
    measured: &MeasuredStore,
    blacklist: &crate::blacklist::Blacklist,
    alive_hosts: &[String],
    stale_secs: u64,
    target_cap: &str,
) -> Option<(String, u64)> {
    let now = time::OffsetDateTime::now_utc();
    let mut best: Option<(usize, String, u64)> = None;
    for ch in &snap.channels {
        if ch.kind == crate::xtream::ChannelKind::Radio {
            continue;
        }
        for src in &ch.sources {
            if src.direct_source.is_some() {
                continue;
            }
            let Some(caps) = variant_caps_required(
                measured, blacklist, src.stream_id, alive_hosts, stale_secs, now,
            ) else {
                continue;
            };
            if !caps.iter().any(|c| c == target_cap) {
                continue;
            }
            let rank = curation.rank_of(&ch.key).unwrap_or(usize::MAX);
            match &best {
                Some((r, _, _)) if *r <= rank => {}
                _ => best = Some((rank, ch.key.clone(), src.stream_id)),
            }
        }
    }
    best.map(|(_, k, sid)| (k, sid))
}

/// Phase 2 helper: per-request channel caps under v2 / per-variant.
///
/// 1. Filter the channel's variants by `caps_required ⊆ client_caps`
///    (treat None client_caps as no-filter — admin / older callers).
/// 2. Skip variants whose v2 scope is empty (stale on every alive host).
/// 3. Pick the rank-winner among survivors using the same scoring the
///    play loop already uses (`source_rank_key_tv`), tying back to the
///    variant's top-ranked host candidate.
/// 4. Return the rank-winner's `caps_required`, or `None` when no
///    variant survives — caller drops the channel from the emit.
pub fn channel_caps_v2(
    state: &crate::state::AppState,
    channel: &CanonicalChannel,
    client_caps: Option<&[String]>,
) -> Option<Vec<String>> {
    let alive = state.hosts.alive_hosts_ranked();
    let stale_secs = state.config.caps_v2_stale_secs;
    let now = time::OffsetDateTime::now_utc();

    if channel.kind == crate::xtream::ChannelKind::Radio {
        // Radio: keep the legacy derivation (per-format) so radio doesn't
        // wait on v2 readiness. `caps_required` (below) handles it.
        let static_caps: Vec<String> = caps_required(channel, &state.measured, &alive)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        return Some(static_caps);
    }

    let mut survivors: Vec<(u64, Vec<String>)> = Vec::new();
    for src in &channel.sources {
        if src.direct_source.is_some() {
            continue;
        }
        let Some(req) = variant_caps_required(
            &state.measured,
            &state.blacklist,
            src.stream_id,
            &alive,
            stale_secs,
            now,
        ) else {
            continue;
        };
        if let Some(caps) = client_caps {
            if !req.iter().all(|c| caps.iter().any(|x| x == c)) {
                continue;
            }
        }
        survivors.push((src.stream_id, req));
    }
    if survivors.is_empty() {
        return None;
    }
    // Pick the rank-winner variant using the same scoring as build_candidates.
    // We approximate by computing the best per-host rank for the variant's
    // best alive host and comparing variants by it.
    let log_snap = state.play_log.snapshot();
    let rank_for = |sid: u64| -> Option<crate::proxy::TvRankKeyOpaque> {
        let mut best: Option<crate::proxy::TvRankKeyOpaque> = None;
        for host in &alive {
            let url = state.xtream.stream_url(host, sid, "m3u8");
            let key = crate::proxy::compute_tv_rank_key(
                &channel.key,
                sid,
                &url,
                host,
                &state.measured,
                &state.blacklist,
                &log_snap,
            );
            match &best {
                Some(b) if b >= &key => {}
                _ => best = Some(key),
            }
        }
        best
    };
    survivors.sort_by(|a, b| {
        rank_for(b.0)
            .cmp(&rank_for(a.0))
    });
    Some(survivors.into_iter().next().map(|(_, caps)| caps).unwrap_or_default())
}

/// Phase 4 helper: list every non-universal cap tag that appears in any
/// variant's `caps_required` under the v2 emit scope. Used to populate
/// the `X-Probes-Expected` header so the client save-guard knows whether
/// every expected tag has resolved before persisting to localStorage.
/// Stale-only tags are excluded — a probe endpoint returning
/// `available:false` for them would otherwise block the save guard
/// indefinitely.
pub fn probes_expected(
    snap: &CatalogSnapshot,
    measured: &MeasuredStore,
    blacklist: &crate::blacklist::Blacklist,
    alive_hosts: &[String],
    stale_secs: u64,
) -> Vec<String> {
    let now = time::OffsetDateTime::now_utc();
    let mut tags: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Universal play probes always advertised.
    tags.insert("live_video_hls".into());
    tags.insert("live_audio_only_hls".into());
    for ch in &snap.channels {
        if ch.kind == crate::xtream::ChannelKind::Radio {
            continue;
        }
        for src in &ch.sources {
            if src.direct_source.is_some() {
                continue;
            }
            let Some(caps) = variant_caps_required(
                measured, blacklist, src.stream_id, alive_hosts, stale_secs, now,
            ) else {
                continue;
            };
            for c in caps {
                // Skip the universal codec tags; those come from canPlayType.
                if matches!(c.as_str(), "hls" | "aac" | "h264" | "live_video_hls" | "live_audio_only_hls") {
                    continue;
                }
                tags.insert(c);
            }
        }
    }
    tags.into_iter().collect()
}

/// Stable hex digest of the cap matrix surface (plan §7). Cheap so it can
/// be recomputed on every `/api/channels` request — it's gated by the cache
/// rebuild check, not called unconditionally.
///
/// Phase 2 input refinement: the digest now also folds the sorted set of
/// cap tags appearing in any variant's `caps_required` across the catalog
/// (v2 scope) so a new tag entering / last variant exiting flips the
/// version; routine measurement updates (a kbps wobble, etc.) don't.
fn compute_version(
    snap: &CatalogSnapshot,
    measured: &MeasuredStore,
    alive_hosts: &[String],
    curation: &Curation,
    blacklist: Option<&crate::blacklist::Blacklist>,
    stale_secs: u64,
    v2: bool,
) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    snap.channels.len().hash(&mut h);
    for target in ProbeTarget::ALL {
        let endpoint = target.endpoint_name();
        let picked = pick_probe_channel(snap, curation, measured, alive_hosts, *target)
            .map(|c| c.key.as_str())
            .unwrap_or("");
        endpoint.hash(&mut h);
        picked.hash(&mut h);
    }
    if v2 {
        if let Some(bl) = blacklist {
            let tags = probes_expected(snap, measured, bl, alive_hosts, stale_secs);
            "v2".hash(&mut h);
            for t in &tags {
                t.hash(&mut h);
            }
        }
    }
    format!("{:016x}", h.finish())
}

#[derive(Debug, Clone, Default)]
struct CacheState {
    per_channel: HashMap<String, Vec<&'static str>>,
    version: String,
    catalog_refreshed_at: Option<OffsetDateTime>,
    measured_generation: u64,
    alive_hosts_hash: u64,
}

/// Lazy per-channel cache. Rebuild fires only when one of the underlying
/// markers (catalog last_refreshed, measured generation, alive-hosts set)
/// has moved since the last build. Reads are cheap (RwLock<>::read clone).
#[derive(Default)]
pub struct CapsRequiredCache {
    state: RwLock<Arc<CacheState>>,
}

/// Read-only snapshot returned by `ensure`. Owns its per-channel map so
/// the caller can iterate without re-acquiring the cache lock.
#[derive(Debug, Clone)]
pub struct CapsSnapshot {
    pub per_channel: HashMap<String, Vec<&'static str>>,
    pub version: String,
}

impl CapsRequiredCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Like the legacy `ensure` but also folds in the v2 scope inputs
    /// (blacklist + stale_secs + v2 flag) so the matrix-version digest
    /// factors them. Old `ensure(...)` callsite removed — v2 inputs
    /// default to (None, 0, false) for callers that don't care.
    pub fn ensure_with_v2(
        &self,
        snap: &CatalogSnapshot,
        measured: &MeasuredStore,
        alive_hosts: &[String],
        tv_curation: &Curation,
        blacklist: Option<&crate::blacklist::Blacklist>,
        stale_secs: u64,
        v2: bool,
    ) -> CapsSnapshot {
        let cur_gen = measured.generation();
        let cur_refreshed = snap.last_refreshed;
        let cur_hosts_hash = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            // Sort indirectly via length + concatenation hash. Alive hosts are
            // already ranked deterministically by `alive_hosts_ranked`.
            alive_hosts.len().hash(&mut h);
            for host in alive_hosts {
                host.hash(&mut h);
            }
            h.finish()
        };
        {
            let g = self.state.read();
            if g.catalog_refreshed_at == cur_refreshed
                && g.measured_generation == cur_gen
                && g.alive_hosts_hash == cur_hosts_hash
            {
                return CapsSnapshot {
                    per_channel: g.per_channel.clone(),
                    version: g.version.clone(),
                };
            }
        }
        // Rebuild outside the read lock — long enough work that we don't
        // want to block concurrent /api/channels readers behind it.
        let mut per_channel = HashMap::with_capacity(snap.channels.len());
        for ch in &snap.channels {
            per_channel.insert(ch.key.clone(), caps_required(ch, measured, alive_hosts));
        }
        let version = compute_version(
            snap,
            measured,
            alive_hosts,
            tv_curation,
            blacklist,
            stale_secs,
            v2,
        );
        let new_state = Arc::new(CacheState {
            per_channel: per_channel.clone(),
            version: version.clone(),
            catalog_refreshed_at: cur_refreshed,
            measured_generation: cur_gen,
            alive_hosts_hash: cur_hosts_hash,
        });
        *self.state.write() = new_state;
        CapsSnapshot { per_channel, version }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::CanonicalSource;
    use crate::measured::{Sample, SampleSource};
    use std::path::PathBuf;

    fn ts_channel(key: &str, stream_id: u64, origin_host: &str) -> CanonicalChannel {
        CanonicalChannel {
            key: key.to_string(),
            name: key.to_string(),
            kind: ChannelKind::Tv,
            sources: vec![CanonicalSource {
                stream_id,
                name: key.to_string(),
                score: 0,
                logo: None,
                tv_archive: false,
                tv_archive_duration: None,
                direct_source: None,
                origin_host: origin_host.to_string(),
                radio_format: None,
            }],
        }
    }

    fn sample(codec: &str, pix_fmt: Option<&str>, dvb_unsafe: Option<bool>) -> Sample {
        Sample {
            at: OffsetDateTime::now_utc(),
            source: SampleSource::Sweep,
            width: 1920,
            height: 1080,
            codec: Some(codec.to_string()),
            pix_fmt: pix_fmt.map(|s| s.to_string()),
            color_transfer: Some("bt709".into()),
            framerate: Some(50.0),
            bitrate_kbps: Some(4500),
            dvb_unsafe,
            sample_rate_hz: None,
            audio_channels: None,
            h264_excess_refs: None,
        }
    }

    fn empty_store() -> MeasuredStore {
        MeasuredStore::load_or_empty(PathBuf::from("/nonexistent-phase6-test.json"))
    }

    #[test]
    fn caps_returns_baseline_for_unmeasured_tv_channel() {
        let ch = ts_channel("rtp1", 1, "http://host.a");
        let m = empty_store();
        let caps = caps_required(&ch, &m, &["http://host.a".into()]);
        assert_eq!(caps, vec!["hls", "h264", "aac", "live_video_hls"]);
    }

    #[test]
    fn caps_swap_h264_for_hevc_when_all_sources_hevc() {
        let ch = ts_channel("hevc-only", 1, "http://host.a");
        let m = empty_store();
        m.push(1, "http://host.a", sample("hevc", Some("yuv420p"), Some(false)));
        let caps = caps_required(&ch, &m, &["http://host.a".into()]);
        assert!(caps.contains(&"hevc"));
        assert!(!caps.contains(&"h264"));
        // hevc_main10 not added — pix_fmt is 8-bit.
        assert!(!caps.contains(&"hevc_main10"));
    }

    #[test]
    fn caps_add_hevc_main10_only_when_all_sources_10_bit_hevc() {
        let ch = ts_channel("hevc-main10", 1, "http://host.a");
        let m = empty_store();
        m.push(1, "http://host.a", sample("hevc", Some("yuv420p10le"), Some(false)));
        let caps = caps_required(&ch, &m, &["http://host.a".into()]);
        assert!(caps.contains(&"hevc_main10"));
        assert!(caps.contains(&"hevc"));
    }

    #[test]
    fn caps_mixed_codec_channel_keeps_baseline() {
        // Two alive hosts, one measured as h264, one as hevc → mixed → no
        // tightening on either axis.
        let ch = ts_channel("mixed", 1, "http://host.a");
        let m = empty_store();
        m.push(1, "http://host.a", sample("h264", Some("yuv420p"), Some(false)));
        m.push(1, "http://host.b", sample("hevc", Some("yuv420p"), Some(false)));
        let caps = caps_required(
            &ch,
            &m,
            &["http://host.a".into(), "http://host.b".into()],
        );
        // Baseline preserved — neither all-hevc nor all-h264-with-something.
        assert!(caps.contains(&"h264"));
        assert!(!caps.contains(&"hevc"));
        assert!(!caps.contains(&"hevc_main10"));
    }

    #[test]
    fn caps_add_dvb_safe_when_all_sources_dvb_unsafe() {
        let ch = ts_channel("dvb-unsafe-only", 1, "http://host.a");
        let m = empty_store();
        m.push(1, "http://host.a", sample("h264", Some("yuv420p"), Some(true)));
        let caps = caps_required(&ch, &m, &["http://host.a".into()]);
        assert!(caps.contains(&"dvb_safe"));
    }

    #[test]
    fn caps_one_dvb_safe_source_does_not_add_cap() {
        // Mixed: one source dvb_unsafe, one strippable → strippable client
        // can play; don't require dvb_safe.
        let ch = ts_channel("mixed-dvb", 1, "http://host.a");
        let m = empty_store();
        m.push(1, "http://host.a", sample("h264", Some("yuv420p"), Some(true)));
        m.push(1, "http://host.b", sample("h264", Some("yuv420p"), Some(false)));
        let caps = caps_required(
            &ch,
            &m,
            &["http://host.a".into(), "http://host.b".into()],
        );
        assert!(!caps.contains(&"dvb_safe"));
    }

    #[test]
    fn cache_rebuilds_when_generation_changes() {
        let mut snap = CatalogSnapshot::empty();
        snap.channels = vec![ts_channel("rtp1", 1, "http://host.a")];
        snap.last_refreshed = Some(OffsetDateTime::now_utc());
        let m = empty_store();
        let curation = Curation::default();
        let cache = CapsRequiredCache::new();
        let hosts = vec!["http://host.a".to_string()];

        let v1 = cache.ensure_with_v2(&snap, &m, &hosts, &curation, None, 0, false).version;
        m.push(1, "http://host.a", sample("hevc", Some("yuv420p10le"), Some(true)));
        let snap2 = cache.ensure_with_v2(&snap, &m, &hosts, &curation, None, 0, false);
        assert!(snap2.per_channel.get("rtp1").unwrap().contains(&"hevc"));
        assert!(snap2.per_channel.get("rtp1").unwrap().contains(&"hevc_main10"));
        assert!(snap2.per_channel.get("rtp1").unwrap().contains(&"dvb_safe"));
        // Version flipped — the picked probe targets changed.
        assert_ne!(v1, snap2.version);
    }

    #[test]
    fn cache_returns_same_version_when_inputs_unchanged() {
        let mut snap = CatalogSnapshot::empty();
        snap.channels = vec![ts_channel("rtp1", 1, "http://host.a")];
        snap.last_refreshed = Some(OffsetDateTime::now_utc());
        let m = empty_store();
        let curation = Curation::default();
        let cache = CapsRequiredCache::new();
        let hosts = vec!["http://host.a".to_string()];
        let v1 = cache.ensure_with_v2(&snap, &m, &hosts, &curation, None, 0, false).version;
        let v2 = cache.ensure_with_v2(&snap, &m, &hosts, &curation, None, 0, false).version;
        assert_eq!(v1, v2);
    }

    #[test]
    fn pick_probe_channel_picks_highest_curation_rank() {
        let mut snap = CatalogSnapshot::empty();
        snap.channels = vec![
            ts_channel("rtp1", 1, "http://host.a"),
            ts_channel("sic", 2, "http://host.a"),
        ];
        snap.last_refreshed = Some(OffsetDateTime::now_utc());
        let m = empty_store();
        m.push(1, "http://host.a", sample("hevc", Some("yuv420p"), Some(false)));
        m.push(2, "http://host.a", sample("hevc", Some("yuv420p"), Some(false)));
        // Curation ranks SIC top — pick must respect that.
        let curation = Curation::from_config(&crate::config::CurationConfig {
            order: vec!["SIC".into(), "RTP 1".into()],
            ..Default::default()
        })
        .unwrap();
        let pick = pick_probe_channel(
            &snap,
            &curation,
            &m,
            &["http://host.a".into()],
            ProbeTarget::Hevc,
        )
        .unwrap();
        assert_eq!(pick.key, "sic");
    }

    #[test]
    fn variant_caps_required_unions_across_alive_hosts() {
        let m = empty_store();
        // Two hosts: A reports h264 only, B reports h264 + h264_excess_refs.
        // Union should carry the excess-refs tag (any-host-needs-it).
        let mut s_a = sample("h264", Some("yuv420p"), Some(false));
        s_a.h264_excess_refs = Some(false);
        s_a.h264_excess_refs = Some(false); // two negatives → OFF
        let mut s_a2 = s_a.clone();
        s_a2.h264_excess_refs = Some(false);
        m.push(7, "http://host.a", s_a);
        m.push(7, "http://host.a", s_a2);
        let mut s_b = sample("h264", Some("yuv420p"), Some(false));
        s_b.h264_excess_refs = Some(true);
        m.push(7, "http://host.b", s_b);
        let bl = crate::blacklist::Blacklist::load_or_empty(
            crate::config::BlacklistConfig {
                host_fail_threshold: 8,
                host_ttl_secs: 300,
                cool_off_steps_secs: [60, 300, 1800, 21600],
                heartbeat_window_secs: 60,
                clean_play_reset_secs: 300,
            },
            std::path::PathBuf::from("/nonexistent-bl-test.json"),
        );
        let hosts = vec!["http://host.a".into(), "http://host.b".into()];
        let caps = variant_caps_required(
            &m, &bl, 7, &hosts, 0, OffsetDateTime::now_utc(),
        )
        .expect("variant has fresh hosts");
        assert!(caps.iter().any(|c| c == "h264_excess_refs"));
        assert!(caps.iter().any(|c| c == "h264"));
    }

    #[test]
    fn variant_caps_required_returns_none_when_all_hosts_stale() {
        let m = empty_store();
        // Push one sample, then ask for fresh-only within a tight TTL.
        let s = sample("h264", Some("yuv420p"), Some(false));
        m.push(8, "http://host.a", s);
        let bl = crate::blacklist::Blacklist::load_or_empty(
            crate::config::BlacklistConfig {
                host_fail_threshold: 8,
                host_ttl_secs: 300,
                cool_off_steps_secs: [60, 300, 1800, 21600],
                heartbeat_window_secs: 60,
                clean_play_reset_secs: 300,
            },
            std::path::PathBuf::from("/nonexistent-bl-test.json"),
        );
        let hosts = vec!["http://host.a".into()];
        let now = OffsetDateTime::now_utc() + time::Duration::seconds(3600);
        // 1-second TTL but 1 hour in the future → stale.
        let res = variant_caps_required(&m, &bl, 8, &hosts, 1, now);
        assert!(res.is_none());
    }

    #[test]
    fn pick_probe_channel_returns_none_when_no_match() {
        let mut snap = CatalogSnapshot::empty();
        snap.channels = vec![ts_channel("rtp1", 1, "http://host.a")];
        snap.last_refreshed = Some(OffsetDateTime::now_utc());
        let m = empty_store();
        // h264 sample, nobody matches Hevc.
        m.push(1, "http://host.a", sample("h264", Some("yuv420p"), Some(false)));
        let curation = Curation::default();
        assert!(pick_probe_channel(
            &snap,
            &curation,
            &m,
            &["http://host.a".into()],
            ProbeTarget::Hevc,
        )
        .is_none());
    }
}
