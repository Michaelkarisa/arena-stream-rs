//! Client for the Laravel backend ("switch6"). Mirrors
//! `com.stream.api.ApiClient`: fire-and-forget writes with exponential
//! backoff, never blocking the media/control path. Talks to the routes
//! defined in `stream_routes.php` (GET/POST/PUT/DELETE, not namespaced
//! under `/api` per the earlier instruction).


//streamkey is the match_id. 
//from the matchdata sent by the user the matchdata.id from the match is == to streamkey.
use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::json;
use std::time::Duration;
use tracing::warn;

pub static CLIENT: Lazy<ApiClient> = Lazy::new(ApiClient::from_env);

const RETRY_DELAYS_MS: [u64; 4] = [500, 1_000, 2_000, 4_000];

pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl ApiClient {
    fn from_env() -> Self {
        let base_url = std::env::var(crate::config::API_BASE_URL_ENV)
            .unwrap_or_else(|_| "http://localhost:8000".to_string());
        let token = std::env::var(crate::config::API_TOKEN_ENV).ok();
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build reqwest client"),
            base_url,
            token,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.http.request(method, url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        req
    }

    /// Fire-and-forget POST/PUT with retry-with-backoff, spawned onto its
    /// own task so callers (control socket, ingest) never block on it.
    fn dispatch(&self, method: reqwest::Method, path: String, body: serde_json::Value) {
        // Laravel's IdempotencyMiddleware (applied to the auth'd goal/status
        // routes) rejects any POST without a UUID `Idempotency-Key`. The key
        // is minted once per logical call — *outside* the retry loop below —
        // so a retry after a timeout replays the same key instead of
        // double-recording the goal.
        let idem_key = (method == reqwest::Method::POST).then(new_idempotency_key);
        let req_base = self.request(method.clone(), &path);
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        drop(req_base);

        tokio::spawn(async move {
            for (attempt, delay_ms) in std::iter::once(0)
                .chain(RETRY_DELAYS_MS.iter().copied())
                .enumerate()
            {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                let url = format!("{base_url}{path}");
                let mut req = http.request(method.clone(), &url).json(&body);
                if let Some(t) = &token {
                    req = req.bearer_auth(t);
                }
                if let Some(k) = &idem_key {
                    req = req.header("Idempotency-Key", k);
                }
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => return,
                    Ok(resp) => warn!(url, status = %resp.status(), "api call non-2xx"),
                    Err(e) => warn!(url, "api call error: {e}"),
                }
            }
            warn!(path, "api call exhausted retries, giving up");
        });
    }

    pub fn create_session(&self, stream_key: &str, targets: &[String]) {
        self.dispatch(
            reqwest::Method::POST,
            "/stream-sessions".into(),
            json!({ "match_id": stream_key, "status": "live", "platform_targets": targets }),
        );
    }

    /// Awaited variant of `create_session` — needed once, at session start,
    /// specifically to capture the Laravel-assigned row id (and the
    /// broadcaster id Laravel resolves from the match's `author_id`) so
    /// later `log_event`/`award_rank_points` calls can reference the real
    /// session instead of a placeholder. Everything else in this client
    /// stays fire-and-forget; this is the one exception, and it only runs
    /// once per session.
    /// `GET /matches/{match_id}` — used at `start` to seed the overlay's
    /// team names/lineup when `register`'s own `matchData`/`otherMatchData`
    /// was absent or too thin (see `control::seed_overlay_from_match_data`
    /// and its `has_usable_team_names` gate). `MatchFormatterService::format`
    /// on the Laravel side returns `homeTeam`/`awayTeam` with `name`,
    /// `formation`, `color`, `startingPlayers`, `substitutes` — the same
    /// field names `Team.toJson()` uses in `models.dart`, so one parser
    /// (`control::parse_team_lineup`) handles either source.
    ///
    /// Awaited like `create_session_awaited`, for the same reason: this
    /// only runs once, at session start, and the overlay has real data to
    /// show only if this resolves before the first frame is drawn.
    pub async fn get_match_awaited(&self, match_id: &str) -> Option<serde_json::Value> {
        for (attempt, delay_ms) in std::iter::once(0).chain(RETRY_DELAYS_MS.iter().copied()).enumerate() {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let resp = match self.request(reqwest::Method::GET, &format!("/matches/{match_id}")).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    warn!(status = %r.status(), match_id, "get_match_awaited: non-2xx");
                    continue;
                }
                Err(e) => {
                    warn!("get_match_awaited: request error: {e}");
                    continue;
                }
            };
            match resp.json::<serde_json::Value>().await {
                Ok(v) => return Some(v),
                Err(e) => warn!("get_match_awaited: bad JSON body: {e}"),
            }
        }
        warn!(match_id, "get_match_awaited: exhausted retries");
        None
    }

    pub async fn create_session_awaited(&self, stream_key: &str, targets: &[String]) -> Option<SessionCreateResult> {
        let body = json!({ "match_id": stream_key, "status": "live", "platform_targets": targets });
        for (attempt, delay_ms) in std::iter::once(0).chain(RETRY_DELAYS_MS.iter().copied()).enumerate() {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let resp = match self.request(reqwest::Method::POST, "/stream-sessions").json(&body).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    warn!(status = %r.status(), "create_session_awaited: non-2xx");
                    continue;
                }
                Err(e) => {
                    warn!("create_session_awaited: request error: {e}");
                    continue;
                }
            };
            match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    if let Some(id) = v.get("id").and_then(serde_json::Value::as_str) {
                        // `broadcaster_id` is the match's `author_id`, resolved
                        // server-side by StreamSessionService::create — absent
                        // (null) if the match has no author on file.
                        let broadcaster_id = v
                            .get("broadcaster_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                        return Some(SessionCreateResult { id: id.to_string(), broadcaster_id });
                    }
                    warn!("create_session_awaited: response had no string 'id' field");
                    return None;
                }
                Err(e) => {
                    warn!("create_session_awaited: failed to parse response JSON: {e}");
                    return None;
                }
            }
        }
        warn!(stream_key, "create_session_awaited: exhausted retries, giving up");
        None
    }

    pub fn update_session_status(&self, stream_key: &str, status: &str) {
        self.dispatch(
            reqwest::Method::PUT,
            format!("/stream-sessions/{stream_key}"),
            json!({ "status": status }),
        );
    }

    pub fn log_event(&self, session_id: &str, event_type: &str, payload: serde_json::Value) {
        self.dispatch(
            reqwest::Method::POST,
            "/stream-events".into(),
            json!({ "session_id": session_id, "type": event_type, "payload": payload }),
        );
    }

    pub fn report_views(
        &self,
        session_id: &str,
        stream_key: &str,
        platform: &str,
        view_count: u64,
    ) {
        self.dispatch(
            reqwest::Method::POST,
            "/stream-views".into(),
            json!({
                "session_id": session_id,
                "stream_key": stream_key,
                "platform": platform,
                "view_count": view_count,
            }),
        );
    }

    pub fn award_rank_points(&self, broadcaster_id: &str, current_views: u64) {
        self.dispatch(
            reqwest::Method::POST,
            format!("/broadcaster-rank/{broadcaster_id}/award"),
            json!({ "current_views": current_views }),
        );
    }

    /// Real Laravel route is `POST /matches/{match}/goals`
    /// (`GoalController::store`) — this used to post to `/add-goal`, a path
    /// that doesn't exist, so every goal silently failed to record. Fixed
    /// alongside `delete_latest_goal` below, its undo counterpart, so the
    /// two don't sit next to each other pointing at one correct and one
    /// wrong path.
    pub fn add_goal(&self, match_id: &str, goal_data: serde_json::Value) {
        self.dispatch(
            reqwest::Method::POST,
            format!("/matches/{match_id}/goals"),
            goal_data,
        );
    }

    /// Undoes whichever goal was scored most recently for this match —
    /// backs the "undoGoal" control action (see control::handle's
    /// "undoGoal" arm and undoLastGoal in services.dart). Laravel resolves
    /// *which* scorer row that is server-side
    /// (`GoalController::destroyLatest`/`GoalService::removeLatestForMatch`),
    /// since this server never captures the `scorer_id` `add_goal`'s own
    /// response would have contained — that call is fire-and-forget, like
    /// every other call in this client except `create_session_awaited`.
    pub fn delete_latest_goal(&self, match_id: &str) {
        self.dispatch(
            reqwest::Method::DELETE,
            format!("/matches/{match_id}/goals/latest"),
            json!({}),
        );
    }

    pub fn update_match_status(&self, match_id: &str, status:i32) {
        self.dispatch(reqwest::Method::POST, format!("/matches/{match_id}/status"), 
        json!({"status":status}));
    }

    /// `GET /ads/select` is the public, unauthenticated endpoint meant for
    /// this — `AdvertisementController::index` (bare `ads`) requires
    /// `$request->user()` and returns the *advertiser's own* ads, not ads
    /// eligible for display. `select` reads `match_id`/`period` from the
    /// query string, not a JSON body, so they're appended to the path
    /// rather than passed as `dispatch`'s `body` argument.
    pub fn get_ads(&self, match_id: &str, period: &str) {
        let path = format!("/ads/select?match_id={match_id}&period={period}");
        self.dispatch(reqwest::Method::GET, path, json!({}));
    }
}



/// Result of `POST /stream-sessions`: the Laravel-assigned session row id,
/// plus the broadcaster id Laravel resolved from the match's `author_id`
/// (see `StreamSessionService::create` — the Rust server has no broadcaster
/// identity of its own, only the match it's streaming).
pub struct SessionCreateResult {
    pub id: String,
    pub broadcaster_id: Option<String>,
}

#[derive(Serialize)]
pub struct ViewTotals {
    pub stream_key: String,
    pub total_views: u64,
}


/// A random RFC-4122 v4 UUID string, built from std only (no `uuid`/`rand`
/// dependency — the project's Cargo.toml isn't part of this source drop, so
/// this avoids assuming either crate is available). `RandomState` is seeded
/// from the OS RNG per instance; mixing two independent instances with the
/// clock and a process-wide counter gives 128 well-distributed bits, which is
/// all an idempotency key needs (uniqueness, not cryptographic strength).
fn new_idempotency_key() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);

    let mut h1 = RandomState::new().build_hasher();
    h1.write_u64(nanos);
    h1.write_u64(n);
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(n);
    h2.write_u64(nanos.rotate_left(17));

    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&h1.finish().to_be_bytes());
    b[8..].copy_from_slice(&h2.finish().to_be_bytes());
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}
