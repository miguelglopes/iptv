//! Radio source loader.
//!
//! Parses a vendored M3U file (e.g. `radios.m3u` from activistpt/IPTV) into
//! `LiveStream` entries tagged `ChannelKind::Radio` and with `direct_source`
//! pointing at the upstream URL. Each entry is tagged with a [`RadioFormat`]
//! derived from the URL shape. The proxy dispatches on the format at play
//! time: HLS goes through the existing manifest-rewriting pipeline; the rest
//! goes through `proxy::play_audio`, which streams upstream bytes directly.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use tracing::warn;

use crate::xtream::{ChannelKind, LiveStream};

/// Audio container/transport format for a radio source. Derived purely from
/// the URL shape at parse time. Drives two decisions downstream:
///   * `api::caps_required(..)` returns format-appropriate caps so a client
///     without `mp3` capability doesn't see a pure-MP3 channel.
///   * `proxy::play_playlist` dispatches `Hls` to the existing playlist
///     rewriter and the rest to `play_audio` (raw bytes pump).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioFormat {
    Hls,
    Mp3,
    Aac,
    /// Audio over HTTP, no extension hint (Icecast/Shoutcast on numbered
    /// ports). Trusted at fetch time via Content-Type.
    Icecast,
    /// `.pls` / `.m3u` indirection — server resolves one hop to the audio
    /// URL before streaming.
    Playlist,
}

/// Classify a radio source URL by its shape alone. Order matters: HLS wins
/// first (`.m3u8` substring), then explicit `.mp3` / `.aac` extensions on
/// the *final filename segment* (so Shoutcast `;listen.pls` metadata flags
/// don't false-trigger Playlist), then `.pls` / `.m3u` playlist files (real
/// indirections), then a permissive `aac` substring catch for streamtheworld
/// (`RFMAAC` carries no extension), and finally `Icecast`.
pub fn classify_url(u: &str) -> RadioFormat {
    let lo = u.to_lowercase();
    if lo.contains(".m3u8") {
        return RadioFormat::Hls;
    }
    let path = lo.split('?').next().unwrap_or(&lo);
    // Only look at the *last filename segment* (after the last `/`). Avoids
    // misclassifying Shoutcast `;listen.pls`-style metadata flags as playlist
    // files (those are audio streams, not .pls indirections).
    let last_seg = path.rsplit('/').next().unwrap_or("");
    // Strip Shoutcast metadata flags (`;stream`, `;listen.pls`, `;stream.nsv`, …)
    // for extension testing. Anything after a `;` is metadata, not the filename.
    let last_seg_pure = last_seg.split(';').next().unwrap_or(last_seg);
    if last_seg_pure.ends_with(".mp3") {
        return RadioFormat::Mp3;
    }
    if last_seg_pure.ends_with(".aac") {
        return RadioFormat::Aac;
    }
    if last_seg_pure.ends_with(".pls") || last_seg_pure.ends_with(".m3u") {
        return RadioFormat::Playlist;
    }
    if path.contains("_aac") || path.contains("aac") {
        return RadioFormat::Aac;
    }
    RadioFormat::Icecast
}

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

/// Load + parse the bundled M3U. Every entry is kept; format classification
/// drives downstream playback dispatch via [`RadioFormat`].
pub fn load_radio_streams(path: &Path) -> Result<Vec<LiveStream>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading radio M3U at {}", path.display()))?;
    let parsed = parse_m3u(&body);
    let mut counts = [0usize; 5];
    for s in &parsed {
        let idx = match s.radio_format.unwrap_or(RadioFormat::Hls) {
            RadioFormat::Hls => 0,
            RadioFormat::Mp3 => 1,
            RadioFormat::Aac => 2,
            RadioFormat::Icecast => 3,
            RadioFormat::Playlist => 4,
        };
        counts[idx] += 1;
    }
    tracing::info!(
        "radio M3U: {} entries parsed (hls={}, mp3={}, aac={}, icecast={}, playlist={})",
        parsed.len(),
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4],
    );
    Ok(parsed)
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
        radio_format: Some(classify_url(url)),
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
    fn parser_keeps_all_entries_and_tags_format() {
        let m3u = r#"#EXTINF:-1,Rádio HLS
https://example.com/stream.m3u8
#EXTINF:-1,Rádio MP3
http://example.com/stream.mp3
#EXTINF:-1,Rádio AAC
http://example.com/stream.aac
#EXTINF:-1,Rádio Pls
http://example.com/playlist.pls
#EXTINF:-1,Rádio Icecast
http://example.com:9000/stream
"#;
        let parsed = parse_m3u(m3u);
        assert_eq!(parsed.len(), 5, "parser keeps all 5");
        assert_eq!(parsed[0].radio_format, Some(RadioFormat::Hls));
        assert_eq!(parsed[1].radio_format, Some(RadioFormat::Mp3));
        assert_eq!(parsed[2].radio_format, Some(RadioFormat::Aac));
        assert_eq!(parsed[3].radio_format, Some(RadioFormat::Playlist));
        assert_eq!(parsed[4].radio_format, Some(RadioFormat::Icecast));
    }

    #[test]
    fn classify_url_table() {
        assert_eq!(classify_url("https://x/stream.m3u8"), RadioFormat::Hls);
        assert_eq!(classify_url("https://x/stream.M3U8?token=1"), RadioFormat::Hls);
        assert_eq!(classify_url("http://x/audio.mp3"), RadioFormat::Mp3);
        assert_eq!(classify_url("http://x/audio.aac"), RadioFormat::Aac);
        // Real .pls indirection (no Shoutcast `;` metadata flag).
        assert_eq!(classify_url("http://x/playlist.pls"), RadioFormat::Playlist);
        assert_eq!(classify_url("http://x/stream.m3u"), RadioFormat::Playlist);
        // Shoutcast metadata flags must not classify as Playlist — those URLs
        // serve audio bytes, not playlist files.
        assert_eq!(
            classify_url("http://centova.radios.pt:8495/;listen.pls"),
            RadioFormat::Icecast,
        );
        assert_eq!(
            classify_url("http://x:8000/;stream.nsv"),
            RadioFormat::Icecast,
        );
        // Streamtheworld redirect: no extension, but path contains `AAC` ⇒ Aac.
        assert_eq!(
            classify_url("https://21313.live.streamtheworld.com/RFMAAC"),
            RadioFormat::Aac,
        );
        // Plain port-based icecast — falls through to Icecast.
        assert_eq!(classify_url("http://centova.radios.pt:9476/"), RadioFormat::Icecast);
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
