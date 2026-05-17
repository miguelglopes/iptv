use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct XtreamClient {
    pub http: Client,
    pub username: String,
    pub password: String,
}

impl XtreamClient {
    pub fn new(username: String, password: String, timeout: Duration) -> Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .user_agent("iptv-proxy/0.1")
            .build()
            .context("building reqwest client")?;
        Ok(Self { http, username, password })
    }

    fn url(&self, host: &str, action: Option<&str>, params: &[(&str, String)]) -> String {
        let mut s = format!(
            "{}/player_api.php?username={}&password={}",
            host.trim_end_matches('/'),
            urlencoding::encode(&self.username),
            urlencoding::encode(&self.password),
        );
        if let Some(a) = action {
            s.push_str("&action=");
            s.push_str(&urlencoding::encode(a));
        }
        for (k, v) in params {
            s.push('&');
            s.push_str(k);
            s.push('=');
            s.push_str(&urlencoding::encode(v));
        }
        s
    }

    pub async fn authenticate(&self, host: &str) -> Result<UserInfo> {
        let url = self.url(host, None, &[]);
        let resp: AuthResponse = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        Ok(resp.user_info)
    }

    pub async fn all_live_streams(&self, host: &str) -> Result<Vec<LiveStream>> {
        let url = self.url(host, Some("get_live_streams"), &[]);
        let mut body: Vec<LiveStream> = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        // Tag each stream with the host it came from so downstream catalog +
        // candidate builders can route each (stream_id) back to the host that
        // actually has it (some providers don't share stream_ids across hosts).
        for s in body.iter_mut() {
            s.origin_host = host.to_string();
        }
        Ok(body)
    }

    pub async fn short_epg(&self, host: &str, stream_id: u64, limit: Option<u32>) -> Result<Vec<EpgProgram>> {
        let mut params = vec![("stream_id", stream_id.to_string())];
        if let Some(n) = limit {
            params.push(("limit", n.to_string()));
        }
        let url = self.url(host, Some("get_short_epg"), &params);
        let v: Value = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        Ok(parse_epg_listings(&v))
    }

    pub async fn simple_data_table(&self, host: &str, stream_id: u64) -> Result<Vec<EpgProgram>> {
        let params = vec![("stream_id", stream_id.to_string())];
        let url = self.url(host, Some("get_simple_data_table"), &params);
        let v: Value = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        Ok(parse_epg_listings(&v))
    }

    pub fn stream_url(&self, host: &str, stream_id: u64, ext: &str) -> String {
        format!(
            "{}/live/{}/{}/{}.{}",
            host.trim_end_matches('/'),
            self.username,
            self.password,
            stream_id,
            ext
        )
    }

    pub fn timeshift_url(
        &self,
        host: &str,
        stream_id: u64,
        duration_min: u32,
        start: OffsetDateTime,
    ) -> String {
        format!(
            "{}/timeshift/{}/{}/{}/{}/{}.m3u8",
            host.trim_end_matches('/'),
            self.username,
            self.password,
            duration_min,
            format_timeshift_start(start),
            stream_id,
        )
    }
}

pub fn format_timeshift_start(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}:{:02}-{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
    )
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    user_info: UserInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    #[serde(default)]
    pub auth: Value,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub active_cons: Option<Value>,
    #[serde(default)]
    pub max_connections: Option<Value>,
    #[serde(default)]
    pub exp_date: Option<Value>,
}

impl UserInfo {
    pub fn is_authenticated(&self) -> bool {
        match &self.auth {
            Value::Number(n) => n.as_u64().map(|v| v > 0).unwrap_or(false),
            Value::String(s) => s == "1" || s.eq_ignore_ascii_case("true"),
            Value::Bool(b) => *b,
            _ => false,
        }
    }

    /// Provider's `max_connections` as a u32. Returns None for missing /
    /// unparseable values, in which case the consumer should pick a sensible
    /// default (2 is typical for Xtream accounts).
    pub fn max_connections_value(&self) -> Option<u32> {
        match self.max_connections.as_ref()? {
            Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
            Value::String(s) => s.parse::<u32>().ok(),
            _ => None,
        }
    }
}

/// Distinguishes TV channels (sourced from Xtream hosts) from radio stations
/// (sourced from a vendored M3U). The whole pipeline — canonicalisation, dedup,
/// blacklist, failover — runs on both kinds identically; this tag exists so the
/// proxy candidate builder picks `direct_source` for radio instead of building
/// an `xtream.stream_url(host, stream_id, ext)`, and so the client can filter
/// the list by mode tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    #[default]
    Tv,
    Radio,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LiveStream {
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub stream_id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub stream_icon: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub category_id: String,
    #[serde(default, deserialize_with = "de_opt_u64_lenient")]
    pub epg_channel_id: Option<u64>,
    #[serde(default)]
    pub added: Option<String>,
    #[serde(default)]
    pub custom_sid: Option<String>,
    #[serde(default)]
    pub tv_archive: Option<Value>,
    #[serde(default, rename = "tv_archive_duration")]
    pub tv_archive_duration: Option<Value>,
    #[serde(default)]
    pub direct_source: Option<String>,
    /// Not present in Xtream JSON — defaults to Tv via `Default`. Radio source
    /// loaders explicitly set this to `Radio` when constructing LiveStream
    /// instances from the vendored M3U.
    #[serde(default, skip)]
    pub kind: ChannelKind,
    /// Which alive host this stream came from. Empty for radio entries (they
    /// carry `direct_source` and don't fan across hosts). Set by
    /// `all_live_streams` at fetch time. Used by `build_canonical` /
    /// `proxy::build_candidates` to route the right stream_id to the right
    /// host on play.
    #[serde(default, skip)]
    pub origin_host: String,
    /// Audio container/transport for radio entries (set by `radio.rs`). None
    /// for TV streams. Drives `caps_required` + the proxy's HLS-vs-audio
    /// dispatch.
    #[serde(default, skip)]
    pub radio_format: Option<crate::radio::RadioFormat>,
}

impl LiveStream {
    pub fn has_tv_archive(&self) -> bool {
        match &self.tv_archive {
            Some(Value::Number(n)) => n.as_u64().map(|v| v > 0).unwrap_or(false),
            Some(Value::String(s)) => s.parse::<u64>().map(|v| v > 0).unwrap_or(false),
            Some(Value::Bool(b)) => *b,
            _ => false,
        }
    }

    pub fn tv_archive_days(&self) -> Option<u32> {
        let v = self.tv_archive_duration.as_ref()?;
        let n = match v {
            Value::Number(n) => n.as_u64()?,
            Value::String(s) => s.parse::<u64>().ok()?,
            _ => return None,
        };
        if n == 0 {
            None
        } else {
            Some(n.min(u32::MAX as u64) as u32)
        }
    }
}

fn de_u64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| serde::de::Error::custom("not u64")),
        Value::String(s) => s.parse::<u64>().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom("expected u64 or string")),
    }
}

fn de_opt_u64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_u64()),
        Value::String(s) if s.is_empty() => Ok(None),
        Value::String(s) => Ok(s.parse::<u64>().ok()),
        _ => Ok(None),
    }
}

fn de_string_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok(String::new()),
        other => Ok(other.to_string()),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EpgProgram {
    pub title: String,
    pub description: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub start: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub end: Option<OffsetDateTime>,
    pub has_archive: bool,
}

fn parse_epg_listings(v: &Value) -> Vec<EpgProgram> {
    let listings = v.get("epg_listings").and_then(|x| x.as_array());
    let arr = match listings {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter().map(parse_epg_program).collect()
}

fn parse_epg_program(p: &Value) -> EpgProgram {
    let start = parse_ts(p, "start_timestamp", "start");
    let end = parse_ts(p, "stop_timestamp", "end");
    let has_archive = parse_bool_field(p, "has_archive");
    EpgProgram {
        title: b64_decode(p.get("title").and_then(|x| x.as_str()).unwrap_or("")),
        description: b64_decode(p.get("description").and_then(|x| x.as_str()).unwrap_or("")),
        start,
        end,
        has_archive,
    }
}

fn parse_bool_field(p: &Value, key: &str) -> bool {
    match p.get(key) {
        Some(Value::Number(n)) => n.as_u64().map(|v| v > 0).unwrap_or(false),
        Some(Value::String(s)) => s.parse::<u64>().map(|v| v > 0).unwrap_or(false),
        Some(Value::Bool(b)) => *b,
        _ => false,
    }
}

fn parse_ts(p: &Value, ts_field: &str, str_field: &str) -> Option<OffsetDateTime> {
    if let Some(t) = p.get(ts_field) {
        if let Some(n) = t.as_u64() {
            return OffsetDateTime::from_unix_timestamp(n as i64).ok();
        }
        if let Some(s) = t.as_str() {
            if let Ok(n) = s.parse::<i64>() {
                return OffsetDateTime::from_unix_timestamp(n).ok();
            }
        }
    }
    if let Some(s) = p.get(str_field).and_then(|x| x.as_str()) {
        if let Ok(dt) = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
            return Some(dt);
        }
        for fmt in [
            time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        ] {
            if let Ok(dt) = time::PrimitiveDateTime::parse(s, fmt) {
                return Some(dt.assume_utc());
            }
        }
    }
    None
}

fn b64_decode(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(s.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s.as_bytes()))
    {
        Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|e| {
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }),
        Err(_) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_epg_program_pulls_has_archive_when_set() {
        let raw = serde_json::json!({
            "id": "1",
            "title": "VGVzdA==",
            "description": "",
            "start": "2026-05-10 02:24:00",
            "end":   "2026-05-10 03:09:00",
            "start_timestamp": "1778379840",
            "stop_timestamp":  "1778382540",
            "has_archive": 1
        });
        let p = parse_epg_program(&raw);
        assert!(p.has_archive, "has_archive should be true for upstream value 1");
    }

    #[test]
    fn parse_epg_program_has_archive_zero_is_false() {
        let raw = serde_json::json!({
            "id": "2",
            "title": "VGVzdA==",
            "description": "",
            "start": "2026-05-10 02:24:00",
            "end":   "2026-05-10 03:09:00",
            "start_timestamp": "1778379840",
            "stop_timestamp":  "1778382540",
            "has_archive": 0
        });
        let p = parse_epg_program(&raw);
        assert!(!p.has_archive);
    }

    #[test]
    fn parse_epg_program_has_archive_missing_is_false() {
        // get_short_epg responses don't include has_archive.
        let raw = serde_json::json!({
            "id": "3",
            "title": "VGVzdA==",
            "start": "2026-05-10 02:24:00",
            "end":   "2026-05-10 03:09:00",
            "start_timestamp": "1778379840",
            "stop_timestamp":  "1778382540"
        });
        let p = parse_epg_program(&raw);
        assert!(!p.has_archive);
    }

    #[test]
    fn parse_epg_listings_pulls_has_archive_for_each_program() {
        let raw = serde_json::json!({
            "epg_listings": [
                {"id": "1", "title": "VA==", "start": "2026-05-10 02:24:00", "end": "2026-05-10 03:09:00",
                 "start_timestamp": "1778379840", "stop_timestamp": "1778382540", "has_archive": 1},
                {"id": "2", "title": "VA==", "start": "2026-05-10 03:09:00", "end": "2026-05-10 04:00:00",
                 "start_timestamp": "1778382540", "stop_timestamp": "1778385600", "has_archive": 0},
            ]
        });
        let ps = parse_epg_listings(&raw);
        assert_eq!(ps.len(), 2);
        assert!(ps[0].has_archive);
        assert!(!ps[1].has_archive);
    }

    #[test]
    fn live_stream_archive_helpers() {
        let s: LiveStream = serde_json::from_value(serde_json::json!({
            "stream_id": 386405,
            "name": "PT: RTP 1 HD ◉",
            "tv_archive": 1,
            "tv_archive_duration": "3"
        })).unwrap();
        assert!(s.has_tv_archive());
        assert_eq!(s.tv_archive_days(), Some(3));

        let s: LiveStream = serde_json::from_value(serde_json::json!({
            "stream_id": 386405,
            "name": "MEO: RTP 1 RAW",
            "tv_archive": 0,
            "tv_archive_duration": "0"
        })).unwrap();
        assert!(!s.has_tv_archive());
        assert_eq!(s.tv_archive_days(), None);

        // Some upstreams return numeric duration, not string.
        let s: LiveStream = serde_json::from_value(serde_json::json!({
            "stream_id": 386405,
            "name": "X",
            "tv_archive": 1,
            "tv_archive_duration": 7
        })).unwrap();
        assert_eq!(s.tv_archive_days(), Some(7));
    }
}

mod urlencoding {
    pub fn encode(s: &str) -> String {
        const HEX: &[u8] = b"0123456789ABCDEF";
        let mut out = String::with_capacity(s.len());
        for &b in s.as_bytes() {
            let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
            if safe {
                out.push(b as char);
            } else {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
        out
    }
}
