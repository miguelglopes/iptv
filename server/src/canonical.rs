use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::xtream::LiveStream;

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
static DISPLAY_FILTER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\u{0020}-\u{007E}\u{00A0}-\u{024F}]").unwrap());
static HAS_ACCENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\u{00C0}-\u{024F}]").unwrap());

static KEY_ALIASES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("btv", "benfica");
    m.insert("cnnpt", "cnnportugal");
    m.insert("panda", "canalpanda");
    m.insert("rtp3", "rtpnoticias");
    m
});

static DISPLAY_OVERRIDES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("rtpnoticias", "RTP Notícias");
    m.insert("benfica", "Benfica TV");
    m.insert("cnnportugal", "CNN Portugal");
    m.insert("canalpanda", "Canal Panda");
    m
});

fn fix_mojibake(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() {
            let pair = &bytes[i..i + 2];
            if pair == [0xC3, 0x83] && i + 3 < bytes.len() {
                let next = &bytes[i + 2..i + 4];
                if next[0] == 0xC2 && (0x80..=0xBF).contains(&next[1]) {
                    let cp = 0xC0u8 | (next[1] & 0x3F);
                    out.push(cp);
                    i += 4;
                    continue;
                }
                if next[0] == 0xC2 {
                    out.push(0xC3);
                    out.push(next[1]);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    match std::str::from_utf8(&out) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(&out).into_owned(),
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

pub fn canonical_key(name: &str) -> String {
    let n = clean(name);
    let n = strip_prefix(&n);
    let n = strip_quality(&n);
    let n = strip_trailing_tv(&n);
    let n = strip_accents(&n);
    let n = n.replace('&', " e ");
    let n = WHITESPACE_RE.replace_all(&n, " ").to_string();
    let n = n.replace('+', " plus ");
    let key: String = n
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    KEY_ALIASES
        .get(key.as_str())
        .map(|s| s.to_string())
        .unwrap_or(key)
}

pub fn display_name(name: &str) -> String {
    let n = clean(name);
    let n = strip_prefix(&n);
    let n = strip_quality(&n);
    let n = DISPLAY_FILTER_RE.replace_all(&n, "");
    WHITESPACE_RE.replace_all(&n, " ").trim().to_string()
}

fn score_variant(name: &str) -> i32 {
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
    if Regex::new(r"(?i)VODAFONE").unwrap().is_match(name) {
        s += 3;
    }
    if Regex::new(r"(?i)MEO").unwrap().is_match(name) {
        s += 2;
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

pub fn build_canonical(streams: &[LiveStream]) -> Vec<CanonicalChannel> {
    let mut groups: HashMap<String, CanonicalChannel> = HashMap::new();
    for st in streams {
        if st.name.is_empty() || st.stream_id == 0 {
            continue;
        }
        if is_separator(&st.name) {
            continue;
        }
        let key = canonical_key(&st.name);
        if key.is_empty() {
            continue;
        }
        let display = display_name(&st.name);
        let score = score_variant(&st.name);
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
            sources: Vec::new(),
        });
        entry.name = prefer_display(&entry.name, &display);
        entry.sources.push(CanonicalSource {
            stream_id: st.stream_id,
            name: st.name.clone(),
            score,
            logo,
            tv_archive,
            tv_archive_duration,
        });
    }

    let mut list: Vec<CanonicalChannel> = groups.into_values().collect();
    for ch in &mut list {
        ch.sources.sort_by(|a, b| b.score.cmp(&a.score));
        if let Some(override_name) = DISPLAY_OVERRIDES.get(ch.key.as_str()) {
            ch.name = (*override_name).to_string();
        }
    }
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls(name: &str, id: u64) -> LiveStream {
        LiveStream {
            stream_id: id,
            name: name.to_string(),
            stream_icon: String::new(),
            category_id: String::new(),
            epg_channel_id: None,
            added: None,
            custom_sid: None,
            tv_archive: None,
            tv_archive_duration: None,
            direct_source: None,
        }
    }

    fn ls_archive(name: &str, id: u64, days: u64) -> LiveStream {
        LiveStream {
            stream_id: id,
            name: name.to_string(),
            stream_icon: String::new(),
            category_id: String::new(),
            epg_channel_id: None,
            added: None,
            custom_sid: None,
            tv_archive: Some(serde_json::json!(1)),
            tv_archive_duration: Some(serde_json::json!(days.to_string())),
            direct_source: None,
        }
    }

    #[test]
    fn benfica_aliases() {
        assert_eq!(canonical_key("BTV"), "benfica");
        assert_eq!(canonical_key("BENFICA TV"), "benfica");
        assert_eq!(canonical_key("BENFICA TV 1"), "benfica");
        assert_eq!(canonical_key("BENFICA TV HD"), "benfica");
    }

    #[test]
    fn rtp3_aliases_to_rtpnoticias() {
        assert_eq!(canonical_key("RTP 3"), "rtpnoticias");
        assert_eq!(canonical_key("RTP Notícias"), "rtpnoticias");
        assert_eq!(canonical_key("MEO: RTP 3 RAW"), "rtpnoticias");
        assert_eq!(canonical_key("PT: RTP 3 HD"), "rtpnoticias");
    }

    #[test]
    fn rtp3_madeira_stays_distinct() {
        assert_eq!(canonical_key("RTP 3 MADEIRA"), "rtp3madeira");
        assert_ne!(canonical_key("RTP 3 MADEIRA"), canonical_key("RTP 3"));
    }

    #[test]
    fn benfica_tv_2_is_distinct() {
        assert_eq!(canonical_key("BENFICA TV"), "benfica");
        assert_ne!(canonical_key("BENFICA TV 2"), canonical_key("BENFICA TV"));
    }

    #[test]
    fn cm_tv_stays_intact() {
        assert_eq!(canonical_key("CM TV"), canonical_key("CMTV"));
    }

    #[test]
    fn quality_collapses() {
        assert_eq!(canonical_key("RTP 1"), canonical_key("RTP 1 HD"));
        assert_eq!(canonical_key("RTP 1 FHD"), canonical_key("RTP 1 RAW"));
        assert_eq!(canonical_key("RTP 1 4K"), canonical_key("RTP 1"));
    }

    #[test]
    fn prefix_strips() {
        assert_eq!(canonical_key("MEO: RTP 1"), canonical_key("VIP: RTP 1"));
        assert_eq!(canonical_key("PT | RTP 1"), canonical_key("RTP 1"));
    }

    #[test]
    fn ampersand_e() {
        assert_eq!(canonical_key("CASA & COZINHA"), canonical_key("CASA E COZINHA"));
    }

    #[test]
    fn plus_distinguishes() {
        assert_ne!(canonical_key("Disney"), canonical_key("Disney+"));
        assert_ne!(canonical_key("Panda"), canonical_key("Panda +"));
    }

    #[test]
    fn separators_dropped() {
        let streams = vec![
            ls("RTP 1", 1),
            ls("##### PORTUGAL #####", 0),
            ls("RTP 1 HD", 2),
        ];
        let cans = build_canonical(&streams);
        assert_eq!(cans.len(), 1);
        assert_eq!(cans[0].sources.len(), 2);
    }

    #[test]
    fn raw_scores_highest() {
        let streams = vec![
            ls("RTP 1 HD", 1),
            ls("RTP 1 RAW", 2),
            ls("RTP 1 FHD", 3),
            ls("RTP 1 4K", 4),
        ];
        let cans = build_canonical(&streams);
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
        let streams = vec![
            ls("MEO: RTP 1 RAW", 1),
            ls_archive("PT: RTP 1 HD ◉", 2, 3),
        ];
        let cans = build_canonical(&streams);
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
        // Some providers set tv_archive=1 but tv_archive_duration="0". Treat as
        // "no usable archive" — we have nothing to ask for.
        let stream = LiveStream {
            stream_id: 99,
            name: "RTP 1 HD".into(),
            stream_icon: String::new(),
            category_id: String::new(),
            epg_channel_id: None,
            added: None,
            custom_sid: None,
            tv_archive: Some(serde_json::json!(1)),
            tv_archive_duration: Some(serde_json::json!("0")),
            direct_source: None,
        };
        let cans = build_canonical(&[stream]);
        assert!(!cans[0].sources[0].tv_archive);
        assert_eq!(cans[0].sources[0].tv_archive_duration, None);
    }
}
