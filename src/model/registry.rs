use super::session::StreamSession;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tracing::info;

/// Server-wide registry mapping stream keys and sender IPs to sessions.
/// Equivalent to `com.stream.model.SessionRegistry` — same two lookup
/// paths (by stream key for control commands, by sender IP for per-packet
/// UDP routing), backed by lock-free concurrent maps instead of
/// `ConcurrentHashMap`.
pub struct SessionRegistry {
    sessions: DashMap<String, Arc<StreamSession>>,
    ip_to_key: DashMap<String, String>,
}

pub static REGISTRY: Lazy<SessionRegistry> = Lazy::new(SessionRegistry::new);

impl SessionRegistry {
    fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            ip_to_key: DashMap::new(),
        }
    }

    pub fn put(&self, key: &str, session: Arc<StreamSession>) {
        self.sessions.insert(key.to_string(), session);
        info!(stream_key = key, "session registered");
    }

    pub fn get(&self, key: &str) -> Option<Arc<StreamSession>> {
        self.sessions.get(key).map(|e| e.value().clone())
    }

    pub fn remove(&self, key: &str) -> Option<Arc<StreamSession>> {
        let removed = self.sessions.remove(key).map(|(_, v)| v);
        if removed.is_some() {
            self.ip_to_key.retain(|_, v| v != key);
            info!(stream_key = key, "session removed");
        }
        removed
    }

    pub fn register_ip(&self, ip: &str, key: &str) {
        self.ip_to_key.insert(ip.to_string(), key.to_string());
    }

    pub fn unregister_ip(&self, ip: &str) {
        self.ip_to_key.remove(ip);
    }

    pub fn session_for_ip(&self, ip: &str) -> Option<Arc<StreamSession>> {
        let key = self.ip_to_key.get(ip)?;
        self.get(key.value())
    }

    pub fn all(&self) -> Vec<Arc<StreamSession>> {
        self.sessions.iter().map(|e| e.value().clone()).collect()
    }
}
