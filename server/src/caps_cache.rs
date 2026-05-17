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

/// Stable hex digest of the cap matrix surface (plan §7). Cheap so it can
/// be recomputed on every `/api/channels` request — it's gated by the cache
/// rebuild check, not called unconditionally.
fn compute_version(
    snap: &CatalogSnapshot,
    measured: &MeasuredStore,
    alive_hosts: &[String],
    curation: &Curation,
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

    /// Rebuild if any of (catalog refreshed timestamp, measured generation,
    /// alive-hosts set) changed; otherwise return the existing snapshot.
    pub fn ensure(
        &self,
        snap: &CatalogSnapshot,
        measured: &MeasuredStore,
        alive_hosts: &[String],
        tv_curation: &Curation,
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
        let version = compute_version(snap, measured, alive_hosts, tv_curation);
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

        let v1 = cache.ensure(&snap, &m, &hosts, &curation).version;
        m.push(1, "http://host.a", sample("hevc", Some("yuv420p10le"), Some(true)));
        let snap2 = cache.ensure(&snap, &m, &hosts, &curation);
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
        let v1 = cache.ensure(&snap, &m, &hosts, &curation).version;
        let v2 = cache.ensure(&snap, &m, &hosts, &curation).version;
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
