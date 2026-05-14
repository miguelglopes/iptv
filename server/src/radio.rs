//! Radio source loader.
//!
//! Parses a vendored M3U file (e.g. `radios.m3u` from activistpt/IPTV) into
//! `LiveStream` entries tagged `ChannelKind::Radio` and with `direct_source`
//! pointing at the upstream HLS URL. From there, everything downstream —
//! canonicalisation, dedup, proxy candidate selection, blacklist accounting,
//! playlist rewriting, segment proxy — runs on the radio entries identically
//! to TV. The `direct_source` field is the *only* discriminator the proxy
//! candidate builder branches on; everything else is shared code.
//!
//! v1 keeps only HLS (`.m3u8`) entries. Raw Icecast / MP3 streams need a
//! content-type-aware pass-through path in `proxy.rs` (the current
//! `fetch_and_rewrite_playlist` bails out on non-EXTM3U content). Adding
//! that is a follow-up; for v1 we drop the ~30 long-tail regionals and keep
//! the ~31 well-known HLS stations.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use tracing::{info, warn};

use crate::xtream::{ChannelKind, LiveStream};

/// Mask reserved for synthetic radio stream IDs. Xtream's real stream IDs are
/// 6-7-digit decimals (well below 2^47), so any ID with the high bit set is
/// unambiguously a radio synthetic. Defensive: keeps the proxy's classifier
/// keyed by `stream_id` from ever picking up a stale TV classification for a
/// radio stream that happens to hash to a colliding low 63-bit value.
const RADIO_ID_MASK: u64 = 0x8000_0000_0000_0000;

static EXTINF_ATTRS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"([\w-]+)="([^"]*)""#).unwrap());

// "Rádio " / "Radio " prefix. Done HERE, before the LiveStream is emitted, so
// the shared `canonical::canonical_key_base` never sees a Rádio-prefixed name
// and therefore can't silently collapse TV channels that legitimately start
// with "Radio …". Accent-insensitive (matches both Rádio and Radio); double
// spaces in the source ("Rádio  Antena 1") collapse via the trailing `\s+`.
static RADIO_PREFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^r[áa]dio\s+").unwrap());
static WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// Load + parse the bundled M3U.
pub fn load_radio_streams(path: &Path) -> Result<Vec<LiveStream>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading radio M3U at {}", path.display()))?;
    let parsed = parse_m3u(&body);
    let total = parsed.len();
    let kept: Vec<_> = parsed.into_iter().filter(is_hls).collect();
    info!(
        "radio M3U: {} entries parsed, {} kept (HLS), {} dropped (non-HLS)",
        total,
        kept.len(),
        total - kept.len(),
    );
    Ok(kept)
}

fn is_hls(stream: &LiveStream) -> bool {
    stream
        .direct_source
        .as_deref()
        .map(|u| u.to_lowercase().contains(".m3u8"))
        .unwrap_or(false)
}

pub fn parse_m3u(body: &str) -> Vec<LiveStream> {
    let mut out = Vec::new();
    let lines: Vec<&str> = body.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if !line.starts_with("#EXTINF:") {
            i += 1;
            continue;
        }
        // Next non-comment, non-empty line is the URL.
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j].trim();
            if !l.is_empty() && !l.starts_with('#') {
                break;
            }
            j += 1;
        }
        if j >= lines.len() {
            break;
        }
        let url = lines[j].trim();
        if let Some(entry) = entry_from(line, url) {
            out.push(entry);
        } else {
            warn!("radio M3U: skipped malformed entry at line {}", i + 1);
        }
        i = j + 1;
    }
    out
}

fn entry_from(extinf_line: &str, url: &str) -> Option<LiveStream> {
    let comma = extinf_line.find(',')?;
    let raw_name = extinf_line[comma + 1..].trim();
    let name = strip_radio_prefix(raw_name);
    if name.is_empty() || url.is_empty() {
        return None;
    }
    let attrs = parse_attrs(&extinf_line[..comma]);
    let stream_icon = attrs.get("tvg-logo").cloned().unwrap_or_default();
    Some(LiveStream {
        stream_id: synth_stream_id(&name, url),
        name,
        stream_icon,
        direct_source: Some(url.to_string()),
        kind: ChannelKind::Radio,
        ..Default::default()
    })
}

fn parse_attrs(prelude: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for cap in EXTINF_ATTRS_RE.captures_iter(prelude) {
        out.insert(cap[1].to_string(), cap[2].to_string());
    }
    out
}

fn strip_radio_prefix(name: &str) -> String {
    let stripped = RADIO_PREFIX_RE.replace(name, "");
    WHITESPACE_RE.replace_all(&stripped, " ").trim().to_string()
}

fn synth_stream_id(name: &str, url: &str) -> u64 {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    url.hash(&mut h);
    RADIO_ID_MASK | (h.finish() & !RADIO_ID_MASK)
}

/// True if `stream_id` came from our synthetic radio namespace. Useful for
/// downstream code that needs to skip Xtream-shaped operations (none today,
/// but cheap insurance).
#[allow(dead_code)]
pub fn is_radio_stream_id(id: u64) -> bool {
    id & RADIO_ID_MASK != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extinf_and_url_pair() {
        let m3u = r#"#EXTINF:-1 tvg-logo="https://example.com/a3.png",Rádio Antena 3
https://streaming-live.rtp.pt/liveradio/antena380a/playlist.m3u8
"#;
        let entries = parse_m3u(m3u);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "Antena 3");
        assert_eq!(
            e.direct_source.as_deref(),
            Some("https://streaming-live.rtp.pt/liveradio/antena380a/playlist.m3u8")
        );
        assert_eq!(e.stream_icon, "https://example.com/a3.png");
        assert!(matches!(e.kind, ChannelKind::Radio));
    }

    #[test]
    fn strips_rádio_prefix_case_and_accent_insensitive() {
        // Both accented and unaccented; trailing double-space collapsed.
        assert_eq!(strip_radio_prefix("Rádio Antena 1"), "Antena 1");
        assert_eq!(strip_radio_prefix("Rádio  Antena 1"), "Antena 1");
        assert_eq!(strip_radio_prefix("RADIO COMERCIAL"), "COMERCIAL");
        assert_eq!(strip_radio_prefix("Radio Foo"), "Foo");
        // Doesn't strip mid-string.
        assert_eq!(strip_radio_prefix("Old Radio Hits"), "Old Radio Hits");
        // No-op if no Rádio prefix.
        assert_eq!(strip_radio_prefix("Antena 1"), "Antena 1");
    }

    #[test]
    fn synth_stream_id_has_high_bit_set() {
        let id = synth_stream_id("Antena 3", "https://example.com/a3.m3u8");
        assert!(id & RADIO_ID_MASK != 0);
        assert!(is_radio_stream_id(id));
        // Real Xtream stream_ids are 6-7-digit decimals.
        assert!(!is_radio_stream_id(386_405));
    }

    #[test]
    fn synth_stream_id_is_deterministic() {
        let a = synth_stream_id("Antena 3", "https://a.example.com/x.m3u8");
        let b = synth_stream_id("Antena 3", "https://a.example.com/x.m3u8");
        let c = synth_stream_id("Antena 3", "https://b.example.com/x.m3u8");
        assert_eq!(a, b);
        assert_ne!(a, c, "different URL → different ID");
    }

    #[test]
    fn hls_filter_drops_icecast_and_mp3() {
        let m3u = r#"#EXTINF:-1,Rádio HLS
https://example.com/stream.m3u8
#EXTINF:-1,Rádio MP3
http://example.com/stream.mp3
#EXTINF:-1,Rádio Icecast
http://example.com:9000/stream
"#;
        let parsed = parse_m3u(m3u);
        assert_eq!(parsed.len(), 3, "parser keeps all 3");
        let kept: Vec<_> = parsed.into_iter().filter(is_hls).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "HLS");
    }

    #[test]
    fn ignores_blank_lines_and_comments_between_extinf_and_url() {
        // Real-world M3Us have leading #EXTM3U, blank lines, and stray
        // comments between EXTINF lines and URLs. Verify we tolerate them.
        let m3u = r#"#EXTM3U

#EXTINF:-1,Rádio Antena 1

# a stray comment
https://example.com/a1.m3u8

#EXTINF:-1,Rádio Antena 2
https://example.com/a2.m3u8
"#;
        let parsed = parse_m3u(m3u);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Antena 1");
        assert_eq!(parsed[1].name, "Antena 2");
    }

    #[test]
    fn missing_url_skipped() {
        // EXTINF at end of file with no URL after → entry skipped, no panic.
        let m3u = r#"#EXTINF:-1,Orphan
"#;
        assert!(parse_m3u(m3u).is_empty());
    }

    #[test]
    fn malformed_extinf_no_comma_skipped() {
        // Some upstreams emit broken EXTINF lines with no comma → skip.
        let m3u = r#"#EXTINF:-1 tvg-logo="x"
https://example.com/stream.m3u8
"#;
        assert!(parse_m3u(m3u).is_empty());
    }

    #[test]
    fn double_space_in_name_collapses() {
        // activistpt's M3U has "Rádio  Antena 1" with two spaces — should
        // canonicalise to "Antena 1".
        let m3u = r#"#EXTINF:-1,Rádio  Antena 1
https://example.com/a1.m3u8
"#;
        let parsed = parse_m3u(m3u);
        assert_eq!(parsed[0].name, "Antena 1");
    }
}
