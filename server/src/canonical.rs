use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::default_order::Curation;
use crate::xtream::{ChannelKind, LiveStream};

static SUPERSCRIPTS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\u{1D00}-\u{1DBF}\u{02B0}-\u{02FF}\u{02C0}-\u{02FF}\u{00B2}\u{00B3}\u{00B9}\u{2070}\u{2071}\u{2074}-\u{209F}\u{207A}-\u{207F}]+").unwrap()
});
static PREFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[\p{Alphabetic}0-9]+\s*[|:]\s*").unwrap());
static QUALITY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(HD|FHD|UHD|4K|RAW|SD|FULL\s*HD|\d{3,4}P)\b").unwrap());
static TRAILING_TV_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s+TV(\s+1)?\s*$").unwrap());
static WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static LEADING_HASH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^#+\s*").unwrap());
static TRAILING_HASH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s*#+$").unwrap());
static SEPARATOR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^#+\s*.*\s*#+\s*$").unwrap());
static ACCENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\p{M}]").unwrap());
// Keep any Unicode letter/mark/number/space/punctuation/math-symbol; drop everything
// else. Preserves non-Latin scripts (Cyrillic, Greek, CJK, ...) AND ASCII math symbols
// like `+` (so "Disney+" displays correctly). The catch-up "◉" marker is \p{So}
// (Other Symbol), not \p{Sm}, so it still gets stripped.
static DISPLAY_FILTER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\p{L}\p{M}\p{N}\p{Zs}\p{P}\p{Sm}]").unwrap());
static HAS_ACCENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\u{00C0}-\u{024F}]").unwrap());

// Generic mojibake repair. Real upstream names mix Latin-1 mojibake bytes (Ã§, Ã©, …)
// with >0xFF noise — superscript HD/RAW markers, catch-up "◉", etc. A whole-string
// gate ("every char ≤0xFF") would refuse to repair those.  Instead we partition the
// string into maximal Latin-1 runs and apply the round-trip repair to each run
// independently; non-Latin-1 chars pass through unchanged.
fn fix_mojibake(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut latin1_buf = String::new();
    for c in s.chars() {
        if (c as u32) <= 0xFF {
            latin1_buf.push(c);
        } else {
            if !latin1_buf.is_empty() {
                out.push_str(&maybe_repair_latin1(&latin1_buf));
                latin1_buf.clear();
            }
            out.push(c);
        }
    }
    if !latin1_buf.is_empty() {
        out.push_str(&maybe_repair_latin1(&latin1_buf));
    }
    out
}

// `s` is guaranteed to contain only chars ≤0xFF. Interpret as Latin-1 bytes and try
// to UTF-8-decode; if the decoded form differs AND introduces non-ASCII letters, the
// run was UTF-8 mis-decoded as Latin-1 (mojibake) — return the decoded form.
fn maybe_repair_latin1(s: &str) -> String {
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    match std::str::from_utf8(&bytes) {
        Ok(decoded)
            if decoded != s
                && decoded
                    .chars()
                    .any(|c| c.is_alphabetic() && !c.is_ascii()) =>
        {
            decoded.to_string()
        }
        _ => s.to_string(),
    }
}

fn strip_accents(s: &str) -> String {
    let decomposed: String = s.nfd().collect();
    ACCENT_RE.replace_all(&decomposed, "").to_string()
}

fn clean(s: &str) -> String {
    let s = fix_mojibake(s);
    let s = SUPERSCRIPTS_RE.replace_all(&s, "");
    let s = LEADING_HASH_RE.replace(&s, "");
    let s = TRAILING_HASH_RE.replace(&s, "");
    let s = WHITESPACE_RE.replace_all(&s, " ");
    s.trim().to_string()
}

fn strip_prefix(s: &str) -> String {
    PREFIX_RE.replace(s, "").trim().to_string()
}

fn strip_quality(s: &str) -> String {
    let s = QUALITY_RE.replace_all(s, "");
    WHITESPACE_RE.replace_all(&s, " ").trim().to_string()
}

fn strip_trailing_tv(s: &str) -> String {
    let stripped = TRAILING_TV_RE.replace(s, "").trim().to_string();
    if stripped.chars().count() < 3 {
        s.to_string()
    } else {
        stripped
    }
}

// Core canonicalization without alias substitution. Used by `Curation::from_config`
// at startup (to canonicalize the order list before the alias map is finalized) and
// internally by `canonical_key_with_aliases`.
fn canonical_key_base(name: &str) -> String {
    let n = clean(name);
    let n = strip_prefix(&n);
    let n = strip_quality(&n);
    let n = strip_trailing_tv(&n);
    let n = strip_accents(&n);
    let n = n.replace('&', " e ");
    let n = WHITESPACE_RE.replace_all(&n, " ").to_string();
    let n = n.replace('+', " plus ");
    n.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

pub fn canonical_key_with_aliases(name: &str, aliases: &HashMap<String, String>) -> String {
    let key = canonical_key_base(name);
    aliases.get(&key).cloned().unwrap_or(key)
}

pub fn canonical_key(name: &str, curation: &Curation) -> String {
    canonical_key_with_aliases(name, &curation.aliases)
}

pub fn display_name(name: &str) -> String {
    let n = clean(name);
    let n = strip_prefix(&n);
    let n = strip_quality(&n);
    let n = DISPLAY_FILTER_RE.replace_all(&n, "");
    WHITESPACE_RE.replace_all(&n, " ").trim().to_string()
}

fn score_variant(name: &str, curation: &Curation) -> i32 {
    let mut s = 0i32;
    if Regex::new(r"(?i)\bRAW\b").unwrap().is_match(name) || name.contains("\u{1D3F}\u{1D2C}\u{1D42}") {
        s += 40;
    }
    if Regex::new(r"(?i)\b(4K|UHD)\b").unwrap().is_match(name) {
        s += 30;
    }
    if Regex::new(r"(?i)\b(FHD|FULL\s*HD)\b").unwrap().is_match(name) {
        s += 20;
    }
    if Regex::new(r"(?i)\bHD\b").unwrap().is_match(name) || name.contains("\u{1D34}\u{1D30}") {
        s += 15;
    }
    if Regex::new(r"(?i)\bSD\b").unwrap().is_match(name) {
        s -= 10;
    }
    if Regex::new(r"(?i)VIP").unwrap().is_match(name) || name.contains("\u{2C7D}\u{1D35}\u{1D3E}") {
        s += 10;
    }
    for (re, delta) in &curation.provider_boosts {
        if re.is_match(name) {
            s += delta;
        }
    }
    s
}

fn is_separator(name: &str) -> bool {
    SEPARATOR_RE.is_match(name.trim())
}

fn prefer_display(a: &str, b: &str) -> String {
    let a_acc = HAS_ACCENT_RE.is_match(a);
    let b_acc = HAS_ACCENT_RE.is_match(b);
    if a_acc && !b_acc {
        return a.to_string();
    }
    if b_acc && !a_acc {
        return b.to_string();
    }
    if b.chars().count() > a.chars().count() {
        b.to_string()
    } else {
        a.to_string()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CanonicalChannel {
    pub key: String,
    pub name: String,
    /// Tv (from Xtream hosts) vs Radio (from vendored M3U). Drives client-side
    /// mode-tab filtering and server-side curation/EPG routing. Set on every
    /// source in `build_canonical`; in practice a single channel is always all
    /// one kind because the canonical_key namespaces "Antena 1" (radio) away
    /// from any TV channel that would have the same key.
    pub kind: ChannelKind,
    pub sources: Vec<CanonicalSource>,
}

impl CanonicalChannel {
    pub fn pick_archive_source(&self) -> Option<&CanonicalSource> {
        self.sources
            .iter()
            .filter(|s| s.tv_archive)
            .max_by_key(|s| s.score)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CanonicalSource {
    pub stream_id: u64,
    pub name: String,
    pub score: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    pub tv_archive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tv_archive_duration: Option<u32>,
    /// Propagated from `LiveStream.direct_source`. None = standard Xtream path
    /// (proxy builds URL from host × stream_id). Some(url) = self-contained
    /// source (radio): proxy uses this URL directly, skipping the host loop.
    /// See `proxy::build_candidates`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_source: Option<String>,
}

pub fn quality_tier(name: &str) -> Option<&'static str> {
    static RE_RAW: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bRAW\b").unwrap());
    static RE_4K: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(4K|UHD)\b").unwrap());
    static RE_FHD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(FHD|FULL\s*HD)\b").unwrap());
    static RE_HD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bHD\b").unwrap());
    static RE_SD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bSD\b").unwrap());
    if RE_RAW.is_match(name) || name.contains("\u{1D3F}\u{1D2C}\u{1D42}") {
        return Some("RAW");
    }
    if RE_4K.is_match(name) {
        return Some("4K");
    }
    if RE_FHD.is_match(name) {
        return Some("FHD");
    }
    if RE_HD.is_match(name) || name.contains("\u{1D34}\u{1D30}") {
        return Some("HD");
    }
    if RE_SD.is_match(name) {
        return Some("SD");
    }
    None
}

pub fn build_canonical(streams: &[LiveStream], curation: &Curation) -> Vec<CanonicalChannel> {
    let mut groups: HashMap<String, CanonicalChannel> = HashMap::new();
    for st in streams {
        if st.name.is_empty() || st.stream_id == 0 {
            continue;
        }
        if is_separator(&st.name) {
            continue;
        }
        let key = canonical_key(&st.name, curation);
        if key.is_empty() {
            continue;
        }
        let display = display_name(&st.name);
        let score = score_variant(&st.name, curation);
        let logo = if st.stream_icon.is_empty() {
            None
        } else {
            Some(st.stream_icon.clone())
        };
        let tv_archive_duration = if st.has_tv_archive() { st.tv_archive_days() } else { None };
        let tv_archive = tv_archive_duration.is_some();
        let entry = groups.entry(key.clone()).or_insert_with(|| CanonicalChannel {
            key: key.clone(),
            name: display.clone(),
            kind: st.kind,
            sources: Vec::new(),
        });
        entry.name = prefer_display(&entry.name, &display);
        // Xtream's `direct_source` is almost always present as `""` (empty
        // string) on live streams — Some("") would route through the
        // direct-URL branch in proxy::build_candidates and try to fetch the
        // empty URL. Coerce empty → None at the canonical boundary so the
        // direct branch only triggers on legitimately populated URLs (today,
        // that's radio entries from radio.rs).
        let direct_source = st.direct_source
            .clone()
            .filter(|s| !s.trim().is_empty());
        entry.sources.push(CanonicalSource {
            stream_id: st.stream_id,
            name: st.name.clone(),
            score,
            logo,
            tv_archive,
            tv_archive_duration,
            direct_source,
        });
    }

    let mut list: Vec<CanonicalChannel> = groups.into_values().collect();
    for ch in &mut list {
        ch.sources.sort_by(|a, b| b.score.cmp(&a.score));
        if let Some(override_name) = curation.display_overrides.get(&ch.key) {
            ch.name = override_name.clone();
        }
    }
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CurationConfig, ProviderBoost};

    fn portugal_curation() -> Curation {
        let cfg = CurationConfig {
            order: Vec::new(),
            aliases: [
                ("btv", "benfica"),
                ("cnnpt", "cnnportugal"),
                ("panda", "canalpanda"),
                ("rtp3", "rtpnoticias"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            display_overrides: [
                ("rtpnoticias", "RTP Notícias"),
                ("benfica", "Benfica TV"),
                ("cnnportugal", "CNN Portugal"),
                ("canalpanda", "Canal Panda"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            provider_boosts: vec![
                ProviderBoost { pattern: "(?i)VODAFONE".into(), score: 3 },
                ProviderBoost { pattern: "(?i)MEO".into(), score: 2 },
            ],
        };
        Curation::from_config(&cfg).expect("fixture should compile")
    }

    fn empty_curation() -> Curation {
        Curation::default()
    }

    fn ls(name: &str, id: u64) -> LiveStream {
        LiveStream {
            stream_id: id,
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn ls_archive(name: &str, id: u64, days: u64) -> LiveStream {
        LiveStream {
            stream_id: id,
            name: name.to_string(),
            tv_archive: Some(serde_json::json!(1)),
            tv_archive_duration: Some(serde_json::json!(days.to_string())),
            ..Default::default()
        }
    }

    #[test]
    fn benfica_aliases() {
        let c = portugal_curation();
        assert_eq!(canonical_key("BTV", &c), "benfica");
        assert_eq!(canonical_key("BENFICA TV", &c), "benfica");
        assert_eq!(canonical_key("BENFICA TV 1", &c), "benfica");
        assert_eq!(canonical_key("BENFICA TV HD", &c), "benfica");
    }

    #[test]
    fn rtp3_aliases_to_rtpnoticias() {
        let c = portugal_curation();
        assert_eq!(canonical_key("RTP 3", &c), "rtpnoticias");
        assert_eq!(canonical_key("RTP Notícias", &c), "rtpnoticias");
        assert_eq!(canonical_key("MEO: RTP 3 RAW", &c), "rtpnoticias");
        assert_eq!(canonical_key("PT: RTP 3 HD", &c), "rtpnoticias");
    }

    #[test]
    fn rtp3_madeira_stays_distinct() {
        let c = portugal_curation();
        assert_eq!(canonical_key("RTP 3 MADEIRA", &c), "rtp3madeira");
        assert_ne!(canonical_key("RTP 3 MADEIRA", &c), canonical_key("RTP 3", &c));
    }

    #[test]
    fn benfica_tv_2_is_distinct() {
        let c = portugal_curation();
        assert_eq!(canonical_key("BENFICA TV", &c), "benfica");
        assert_ne!(canonical_key("BENFICA TV 2", &c), canonical_key("BENFICA TV", &c));
    }

    #[test]
    fn cm_tv_stays_intact() {
        let c = empty_curation();
        assert_eq!(canonical_key("CM TV", &c), canonical_key("CMTV", &c));
    }

    #[test]
    fn quality_collapses() {
        let c = empty_curation();
        assert_eq!(canonical_key("RTP 1", &c), canonical_key("RTP 1 HD", &c));
        assert_eq!(canonical_key("RTP 1 FHD", &c), canonical_key("RTP 1 RAW", &c));
        assert_eq!(canonical_key("RTP 1 4K", &c), canonical_key("RTP 1", &c));
    }

    #[test]
    fn prefix_strips() {
        let c = empty_curation();
        assert_eq!(canonical_key("MEO: RTP 1", &c), canonical_key("VIP: RTP 1", &c));
        assert_eq!(canonical_key("PT | RTP 1", &c), canonical_key("RTP 1", &c));
    }

    #[test]
    fn ampersand_e() {
        let c = empty_curation();
        assert_eq!(
            canonical_key("CASA & COZINHA", &c),
            canonical_key("CASA E COZINHA", &c)
        );
    }

    #[test]
    fn plus_distinguishes() {
        let c = empty_curation();
        assert_ne!(canonical_key("Disney", &c), canonical_key("Disney+", &c));
        assert_ne!(canonical_key("Panda", &c), canonical_key("Panda +", &c));
    }

    #[test]
    fn separators_dropped() {
        let c = empty_curation();
        let streams = vec![
            ls("RTP 1", 1),
            ls("##### PORTUGAL #####", 0),
            ls("RTP 1 HD", 2),
        ];
        let cans = build_canonical(&streams, &c);
        assert_eq!(cans.len(), 1);
        assert_eq!(cans[0].sources.len(), 2);
    }

    #[test]
    fn raw_scores_highest() {
        let c = empty_curation();
        let streams = vec![
            ls("RTP 1 HD", 1),
            ls("RTP 1 RAW", 2),
            ls("RTP 1 FHD", 3),
            ls("RTP 1 4K", 4),
        ];
        let cans = build_canonical(&streams, &c);
        assert_eq!(cans.len(), 1);
        let names: Vec<_> = cans[0].sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names[0], "RTP 1 RAW");
        assert!(names[1] == "RTP 1 4K");
    }

    #[test]
    fn mojibake_recovers() {
        let n = display_name("BENFICAÃ ÃO");
        assert!(!n.contains("Ã§"), "got: {n}");
    }

    #[test]
    fn mojibake_round_trip_repairs_doubly_encoded_name() {
        // UTF-8 for "RTP Notícias" mis-decoded as Latin-1 produces "RTP NotÃ\u{00AD}cias"
        // (because í = U+00ED encodes to bytes [0xC3, 0xAD], which as Latin-1 read as
        // Ã + soft-hyphen). The generic detector should round-trip it back to the
        // proper UTF-8.
        let bytes = "RTP Notícias".as_bytes();
        let mojibaked: String = bytes.iter().map(|&b| b as char).collect();
        assert_ne!(mojibaked, "RTP Notícias", "fixture should differ from clean form");
        let fixed = fix_mojibake(&mojibaked);
        assert_eq!(fixed, "RTP Notícias");
    }

    #[test]
    fn mojibake_detector_leaves_clean_text_alone() {
        // Already-correct UTF-8 with diacritics must not be touched.
        assert_eq!(fix_mojibake("RTP Notícias"), "RTP Notícias");
        assert_eq!(fix_mojibake("Açores"), "Açores");
        // Pure ASCII: round-trips identical, no non-ASCII alpha present, leave alone.
        assert_eq!(fix_mojibake("RTP 1"), "RTP 1");
        // Non-Latin scripts: each non-Latin-1 char is its own pass-through; the
        // surrounding ASCII runs round-trip identically and are left alone.
        assert_eq!(fix_mojibake("Россия"), "Россия");
        assert_eq!(fix_mojibake("Țara"), "Țara");
    }

    #[test]
    fn mojibake_round_trip_repairs_mixed_with_superscript_markers() {
        // Regression for issue #1: real upstream names mix mojibake bytes with
        // non-Latin-1 markers like the superscript "ᴴᴰ" suffix. The old whole-string
        // gate refused to repair these (any >0xFF char would cause an early no-op),
        // so two channels surfaced as duplicates under keys "caaaepesca" /
        // "caaavision" instead of merging into "cacaepesca" / "cacavision".
        //
        // The Latin-1-run partition repairs each maximal Latin-1 run independently,
        // leaving the superscripts in place for SUPERSCRIPTS_RE in clean() to strip.
        let c = empty_curation();

        // Direct fix_mojibake check.
        let mojibaked = "VIP: CAÃ§A E PESCA \u{1D34}\u{1D30}";
        let fixed = fix_mojibake(mojibaked);
        assert!(fixed.contains("CAçA E PESCA"), "got: {fixed:?}");
        assert!(
            fixed.contains("\u{1D34}\u{1D30}"),
            "superscripts must pass through fix_mojibake: {fixed:?}"
        );

        // End-to-end: the mojibaked + superscript name must canonicalize to the
        // same key as the clean variant (so build_canonical merges them).
        let clean_key = canonical_key("CAÇA E PESCA", &c);
        let dirty_key = canonical_key("VIP: CAÃ§A E PESCA \u{1D34}\u{1D30}", &c);
        assert_eq!(clean_key, "cacaepesca");
        assert_eq!(dirty_key, clean_key);

        // And the parallel CAÇAVISION pair from R1's report.
        assert_eq!(
            canonical_key("CAÇAVISION", &c),
            canonical_key("VIP: CAÃ§AVISION \u{1D34}\u{1D30}", &c)
        );

        // build_canonical merges the two variants under one entry.
        let streams = vec![
            ls("CAÇA E PESCA", 1),
            ls("VIP: CAÃ§A E PESCA \u{1D34}\u{1D30}", 2),
        ];
        let cans = build_canonical(&streams, &c);
        assert_eq!(cans.len(), 1, "expected one merged channel, got {cans:?}");
        assert_eq!(cans[0].sources.len(), 2);
    }

    #[test]
    fn quality_tier_known_provider_names() {
        // From the catch-up provider data: only PT: feeds are marked ◉.
        assert_eq!(quality_tier("PT: RTP 1 HD ◉"), Some("HD"));
        assert_eq!(quality_tier("PT: SIC 4K ◉"), Some("4K"));
        assert_eq!(quality_tier("PT: TVI 4K ◉"), Some("4K"));
        assert_eq!(quality_tier("PT: NEWS NOW HD ◉"), Some("HD"));
    }

    #[test]
    fn quality_tier_priority_raw_over_4k() {
        // RAW always wins. (No catch-up source has RAW today, but score order matters
        // when ranking sources for live.)
        assert_eq!(quality_tier("MEO: RTP 1 RAW"), Some("RAW"));
        assert_eq!(quality_tier("RTP 1 4K"), Some("4K"));
        assert_eq!(quality_tier("RTP 1 UHD"), Some("4K"));
        assert_eq!(quality_tier("RTP 1 FULL HD"), Some("FHD"));
        assert_eq!(quality_tier("RTP 1 FHD"), Some("FHD"));
        assert_eq!(quality_tier("RTP 1 HD"), Some("HD"));
        assert_eq!(quality_tier("RTP 1 SD"), Some("SD"));
    }

    #[test]
    fn quality_tier_missing() {
        // Plain name without a quality token: None. Mostly happens for
        // category separators or the rare unlabelled stream.
        assert_eq!(quality_tier("RTP 1"), None);
        assert_eq!(quality_tier("BENFICA TV"), None);
    }

    #[test]
    fn build_canonical_propagates_archive_flags() {
        let c = empty_curation();
        let streams = vec![
            ls("MEO: RTP 1 RAW", 1),
            ls_archive("PT: RTP 1 HD ◉", 2, 3),
        ];
        let cans = build_canonical(&streams, &c);
        assert_eq!(cans.len(), 1);
        let ch = &cans[0];
        let archive_sources: Vec<&CanonicalSource> = ch
            .sources
            .iter()
            .filter(|s| s.tv_archive)
            .collect();
        assert_eq!(archive_sources.len(), 1);
        assert_eq!(archive_sources[0].stream_id, 2);
        assert_eq!(archive_sources[0].tv_archive_duration, Some(3));
        // The RAW (non-archive) source still ranks first for live.
        assert!(ch.sources[0].name.contains("RAW"));
        assert!(!ch.sources[0].tv_archive);
    }

    #[test]
    fn build_canonical_duration_zero_clears_flag() {
        let c = empty_curation();
        // Some providers set tv_archive=1 but tv_archive_duration="0". Treat as
        // "no usable archive" — we have nothing to ask for.
        let stream = LiveStream {
            stream_id: 99,
            name: "RTP 1 HD".into(),
            tv_archive: Some(serde_json::json!(1)),
            tv_archive_duration: Some(serde_json::json!("0")),
            ..Default::default()
        };
        let cans = build_canonical(&[stream], &c);
        assert!(!cans[0].sources[0].tv_archive);
        assert_eq!(cans[0].sources[0].tv_archive_duration, None);
    }

    #[test]
    fn empty_curation_no_ranking_no_aliases_no_overrides() {
        // With empty curation: no aliases (BTV stays distinct from Benfica), no
        // display overrides (the name is whatever prefer_display picks), no provider
        // boosts (MEO/VODAFONE bonuses absent), no rank entries.
        let c = empty_curation();
        assert_eq!(canonical_key("BTV", &c), "btv");
        assert_ne!(canonical_key("BTV", &c), canonical_key("BENFICA TV", &c));
        assert_eq!(c.rank_of("rtp1"), None);
        assert_eq!(c.rank_of("anything"), None);
        // score_variant under empty curation: just universal quality / VIP.
        assert_eq!(score_variant("RTP 1 HD", &c), 15);
        assert_eq!(score_variant("MEO: RTP 1 HD", &c), 15); // no MEO boost
        assert_eq!(score_variant("VODAFONE RTP 1 HD", &c), 15); // no VODAFONE boost
    }

    #[test]
    fn populated_curation_provider_boosts_apply() {
        let c = portugal_curation();
        assert_eq!(score_variant("RTP 1 HD", &c), 15);
        assert_eq!(score_variant("MEO: RTP 1 HD", &c), 15 + 2);
        assert_eq!(score_variant("VODAFONE RTP 1 HD", &c), 15 + 3);
    }

    #[test]
    fn display_filter_keeps_non_latin_letters() {
        // Cyrillic, Greek, Romanian, CJK — all should survive into the display name.
        assert_eq!(display_name("Россия 1"), "Россия 1");
        assert_eq!(display_name("ΕΡΤ 1"), "ΕΡΤ 1");
        assert_eq!(display_name("Țara"), "Țara");
        assert_eq!(display_name("中央電視台"), "中央電視台");
        // Catch-up "◉" marker is \p{So} (Other Symbol) — stripped.
        assert_eq!(display_name("RTP 1 ◉"), "RTP 1");
        // ASCII math symbols (\p{Sm}) are preserved — "Disney+" must survive.
        assert_eq!(display_name("Disney+"), "Disney+");
    }

    #[test]
    fn display_filter_preserves_plus_strips_other_symbols() {
        // Regression for issue #2: \p{Sm} keeps the math-symbol "+" while \p{So}
        // (catch-up "◉", stars "★", etc.) and \p{Sc} (currency) are still stripped.
        assert_eq!(display_name("Disney+"), "Disney+");
        assert_eq!(display_name("Panda + Kids"), "Panda + Kids");
        assert_eq!(display_name("RTP 1 ◉"), "RTP 1");
        assert_eq!(display_name("★ HBO"), "HBO");
    }

    #[test]
    fn populated_curation_orders_via_rank() {
        // Build a small fixture with an explicit order; rank_of must map canonical
        // keys (post-alias) back to ordinals.
        let cfg = CurationConfig {
            order: vec!["RTP 1".into(), "BTV".into(), "SIC".into()],
            aliases: [("btv".to_string(), "benfica".to_string())]
                .into_iter()
                .collect(),
            display_overrides: HashMap::new(),
            provider_boosts: Vec::new(),
        };
        let c = Curation::from_config(&cfg).unwrap();
        assert_eq!(c.rank_of("rtp1"), Some(0));
        assert_eq!(c.rank_of("benfica"), Some(1)); // BTV → benfica via alias
        assert_eq!(c.rank_of("sic"), Some(2));
        assert_eq!(c.rank_of("unknown"), None);
    }
}
