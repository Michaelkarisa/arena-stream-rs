//! Live viewer counts from the platforms a session is streaming to.
//!
//! The client's `register` command carries a `urls` map of the social
//! platforms it is going live on (e.g. `{"youtube": "<key or rtmp url>",
//! "facebook": "<key or rtmp url>"}`); each value's ingest *stream key* is what
//! identifies the session's live video on that platform. A stream key is not a
//! video id, so each platform needs one lookup to resolve it:
//!
//! * **YouTube** — `liveStreams.list` finds the stream whose
//!   `cdn.ingestionInfo.streamName` is the key, `liveBroadcasts.list` finds
//!   the active broadcast bound to it (its id is the video id), and
//!   `videos.list` returns `liveStreamingDetails.concurrentViewers`. These
//!   need the channel owner's OAuth credentials (see `config::YOUTUBE_*`).
//! * **Facebook** — `/{page}/live_videos` lists the page's live videos with
//!   their `stream_url`; the `LIVE` one whose url contains the key has the
//!   `live_views` count. Needs `config::FACEBOOK_ACCESS_TOKEN_ENV`.
//!
//! Counts are polled by [`spawn_poller`] every `VIEWER_POLL_INTERVAL_MS` into
//! `StreamSession::platform_views`; `main::inactivity_reaper` reports them.
//! A platform with no credentials configured, or whose broadcast isn't live
//! yet, simply has no (or a zero) count — nothing is invented.

use crate::config;
use crate::model::{StreamSession, REGISTRY};
use anyhow::{anyhow, Context};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    YouTube,
    Facebook,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::YouTube => "youtube",
            Self::Facebook => "facebook",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        if n.contains("youtube") || n == "yt" {
            Some(Self::YouTube)
        } else if n.contains("facebook") || n == "fb" {
            Some(Self::Facebook)
        } else {
            None
        }
    }

    fn from_url(url: &str) -> Option<Self> {
        let u = url.to_ascii_lowercase();
        if u.contains("youtube.com") || u.contains("youtu.be") {
            Some(Self::YouTube)
        } else if u.contains("facebook.com") || u.contains("fb.com") {
            Some(Self::Facebook)
        } else {
            None
        }
    }
}

/// Bare stream key from either a plain key or a full RTMP(S) url
/// (`rtmp://a.rtmp.youtube.com/live2/abcd-efgh`), dropping any `?query`.
pub fn extract_stream_key(value: &str) -> String {
    let no_query = value.trim().split('?').next().unwrap_or("");
    no_query.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

/// Reads the `urls` value of a `register` command into `(platform, key)`
/// pairs. The platform is taken from the map's key ("youtube"/"facebook"),
/// or from the url's host when the label isn't recognisable; a plain array
/// of urls works too. Entries that match neither platform are ignored.
pub fn parse_urls(urls: &Value) -> Vec<(Platform, String)> {
    let mut out = Vec::new();
    let mut push = |platform: Option<Platform>, raw: &str| {
        let key = extract_stream_key(raw);
        if let (Some(p), false) = (platform, key.is_empty()) {
            out.push((p, key));
        }
    };
    match urls {
        Value::Object(map) => {
            for (label, v) in map {
                if let Some(raw) = v.as_str() {
                    push(Platform::from_name(label).or_else(|| Platform::from_url(raw)), raw);
                }
            }
        }
        Value::Array(items) => {
            for raw in items.iter().filter_map(Value::as_str) {
                push(Platform::from_url(raw), raw);
            }
        }
        _ => {}
    }
    out
}

/// Polls viewer counts for `session` until the session is gone.
pub fn spawn_poller(session: Arc<StreamSession>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(config::VIEWER_POLL_INTERVAL_MS));
        loop {
            interval.tick().await;
            match REGISTRY.get(&session.stream_key) {
                Some(cur) if Arc::ptr_eq(&cur, &session) => {}
                _ => break,
            }
            // Snapshot first: never hold a DashMap guard across an await.
            let keys: Vec<(String, String)> = session
                .platform_keys
                .iter()
                .map(|e| (e.key().clone(), e.value().clone()))
                .collect();
            for (platform, key) in keys {
                let result = match platform.as_str() {
                    "youtube" => youtube_viewers(&key).await,
                    "facebook" => facebook_viewers(&key).await,
                    _ => continue,
                };
                match result {
                    Ok(count) => {
                        session.platform_views.insert(platform, count.unwrap_or(0));
                    }
                    // Keep the previous count on a failed poll.
                    Err(e) => warn!(stream_key = %session.stream_key, platform, "viewer poll failed: {e:#}"),
                }
            }
        }
    });
}

static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .expect("failed to build reqwest client")
});

// ── YouTube ──────────────────────────────────────────────────────────────

const YT_API: &str = "https://www.googleapis.com/youtube/v3";

static YT_TOKEN: Lazy<tokio::sync::Mutex<Option<(String, Instant)>>> = Lazy::new(|| tokio::sync::Mutex::new(None));
/// stream key -> `liveStream` id (stable for a stream key, so looked up once).
static YT_STREAM_IDS: Lazy<DashMap<String, String>> = Lazy::new(DashMap::new);

async fn youtube_token() -> anyhow::Result<String> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if let (Some(id), Some(secret), Some(refresh)) = (
        env(config::YOUTUBE_CLIENT_ID_ENV),
        env(config::YOUTUBE_CLIENT_SECRET_ENV),
        env(config::YOUTUBE_REFRESH_TOKEN_ENV),
    ) {
        let mut cached = YT_TOKEN.lock().await;
        if let Some((tok, expires)) = cached.as_ref() {
            if Instant::now() < *expires {
                return Ok(tok.clone());
            }
        }
        let resp: Value = HTTP
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", id.as_str()),
                ("client_secret", secret.as_str()),
                ("refresh_token", refresh.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?
            .error_for_status()
            .context("youtube token refresh")?
            .json()
            .await?;
        let tok = resp
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("token response had no access_token"))?
            .to_string();
        let ttl = resp.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).saturating_sub(60);
        *cached = Some((tok.clone(), Instant::now() + Duration::from_secs(ttl)));
        return Ok(tok);
    }
    env(config::YOUTUBE_ACCESS_TOKEN_ENV).ok_or_else(|| {
        anyhow!(
            "no YouTube credentials: set {} or {}/{}/{}",
            config::YOUTUBE_ACCESS_TOKEN_ENV,
            config::YOUTUBE_CLIENT_ID_ENV,
            config::YOUTUBE_CLIENT_SECRET_ENV,
            config::YOUTUBE_REFRESH_TOKEN_ENV
        )
    })
}

async fn yt_get(token: &str, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Value> {
    Ok(HTTP
        .get(format!("{YT_API}/{path}"))
        .bearer_auth(token)
        .query(query)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("youtube {path}"))?
        .json()
        .await?)
}

/// `Ok(None)` = no active broadcast for this key (not live yet / ended).
async fn youtube_viewers(stream_key: &str) -> anyhow::Result<Option<u64>> {
    let token = youtube_token().await?;

    let stream_id = match YT_STREAM_IDS.get(stream_key).map(|e| e.value().clone()) {
        Some(id) => id,
        None => {
            let streams = yt_get(&token, "liveStreams", &[("part", "id,cdn"), ("mine", "true"), ("maxResults", "50")]).await?;
            let found = streams["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| s["cdn"]["ingestionInfo"]["streamName"].as_str() == Some(stream_key))
                .and_then(|s| s["id"].as_str().map(String::from));
            let Some(id) = found else {
                return Err(anyhow!("no YouTube live stream on this channel uses the supplied stream key"));
            };
            YT_STREAM_IDS.insert(stream_key.to_string(), id.clone());
            id
        }
    };

    let broadcasts = yt_get(
        &token,
        "liveBroadcasts",
        &[("part", "id,contentDetails"), ("broadcastStatus", "active"), ("maxResults", "50")],
    )
    .await?;
    let video_id = broadcasts["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["contentDetails"]["boundStreamId"].as_str() == Some(stream_id.as_str()))
        .and_then(|b| b["id"].as_str().map(String::from));
    let Some(video_id) = video_id else { return Ok(None) };

    let videos = yt_get(&token, "videos", &[("part", "liveStreamingDetails"), ("id", &video_id)]).await?;
    // `concurrentViewers` is a string in the API, and absent when the
    // broadcaster hides the count or nobody is watching yet.
    Ok(videos["items"][0]["liveStreamingDetails"]["concurrentViewers"]
        .as_str()
        .and_then(|s| s.parse().ok()))
}

// ── Facebook ─────────────────────────────────────────────────────────────

async fn facebook_viewers(stream_key: &str) -> anyhow::Result<Option<u64>> {
    let token = std::env::var(config::FACEBOOK_ACCESS_TOKEN_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("no Facebook credentials: set {}", config::FACEBOOK_ACCESS_TOKEN_ENV))?;
    let page = std::env::var(config::FACEBOOK_PAGE_ID_ENV).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "me".into());
    let version = std::env::var(config::FACEBOOK_GRAPH_VERSION_ENV).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "v21.0".into());

    let resp: Value = HTTP
        .get(format!("https://graph.facebook.com/{version}/{page}/live_videos"))
        .bearer_auth(token)
        .query(&[("fields", "id,status,stream_url,secure_stream_url,live_views"), ("limit", "25")])
        .send()
        .await?
        .error_for_status()
        .context("facebook live_videos")?
        .json()
        .await?;

    let live = resp["data"].as_array().into_iter().flatten().find(|v| {
        v["status"].as_str() == Some("LIVE")
            && ["stream_url", "secure_stream_url"]
                .iter()
                .any(|f| v[*f].as_str().is_some_and(|u| u.contains(stream_key)))
    });
    Ok(live.map(|v| v["live_views"].as_u64().unwrap_or(0)))
}

/// Logged once at startup so a missing credential is visible immediately
/// rather than as a stream of per-poll warnings.
pub fn log_configuration() {
    let set = |n: &str| std::env::var(n).map(|v| !v.is_empty()).unwrap_or(false);
    let yt = set(config::YOUTUBE_ACCESS_TOKEN_ENV)
        || (set(config::YOUTUBE_CLIENT_ID_ENV) && set(config::YOUTUBE_CLIENT_SECRET_ENV) && set(config::YOUTUBE_REFRESH_TOKEN_ENV));
    let fb = set(config::FACEBOOK_ACCESS_TOKEN_ENV);
    info!(youtube = yt, facebook = fb, "social viewer-count credentials");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_extraction() {
        assert_eq!(extract_stream_key("abcd-efgh"), "abcd-efgh");
        assert_eq!(extract_stream_key("rtmp://a.rtmp.youtube.com/live2/abcd-efgh"), "abcd-efgh");
        assert_eq!(extract_stream_key("rtmps://live-api-s.facebook.com:443/rtmp/FB-1-0-Xy?s_bl=1&s_sc=2"), "FB-1-0-Xy");
    }

    #[test]
    fn urls_map_by_label_or_host() {
        let v = json!({
            "youtube": "rtmp://a.rtmp.youtube.com/live2/yt-key",
            "Facebook": "fb-key",
            "custom": "rtmps://live-api-s.facebook.com:443/rtmp/fb2",
            "twitch": "rtmp://live.twitch.tv/app/tw"
        });
        let mut got: Vec<_> = parse_urls(&v).into_iter().map(|(p, k)| (p.as_str(), k)).collect();
        got.sort();
        assert_eq!(got, vec![("facebook", "fb-key".into()), ("facebook", "fb2".into()), ("youtube", "yt-key".into())]);
    }
}
