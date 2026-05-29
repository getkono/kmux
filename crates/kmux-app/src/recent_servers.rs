use std::time::{SystemTime, UNIX_EPOCH};

use kmux_protocol::messages::SessionEntry;
use serde::{Deserialize, Serialize};

const MAX_SERVERS: usize = 10;
const CACHE_FILE: &str = "recent_servers.json";

/// How the client connects to this server.
///
/// Cache entries written by older builds may contain a `Direct { host, port }`
/// variant. Those entries fail to deserialize; the loader falls back to an
/// empty cache, which is harmless — the user simply loses their recent-servers
/// list and rebuilds it on next use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerKind {
    Local,
    Ssh {
        user: Option<String>,
        host: String,
        ssh_port: Option<u16>,
    },
}

/// A single session snapshot cached from a previous connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSession {
    pub word_id: String,
    pub name: String,
    pub cwd: String,
}

/// A single entry in the recent-servers cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentServer {
    /// Cache key. `""` for local daemon, `"user@host"` for SSH, `"host:port"` for direct.
    pub server_string: String,
    /// Human-readable label shown in the badge and picker.
    pub display: String,
    /// Unix timestamp (seconds) of last successful authentication.
    pub last_used: u64,
    /// Session list snapshot from the last successful connection.
    pub sessions: Vec<CachedSession>,
    /// Connection kind used to reconnect.
    pub kind: ServerKind,
}

impl RecentServer {
    /// Returns a human-readable relative time string (e.g. "5s ago", "2h ago").
    pub fn time_ago(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let diff = now.saturating_sub(self.last_used);
        if diff < 60 {
            format!("{diff}s ago")
        } else if diff < 3600 {
            format!("{}m ago", diff / 60)
        } else if diff < 86400 {
            format!("{}h ago", diff / 3600)
        } else {
            format!("{}d ago", diff / 86400)
        }
    }
}

/// Persistent local cache of recently connected servers.
pub struct RecentServersCache {
    servers: Vec<RecentServer>,
}

impl RecentServersCache {
    /// Load the cache from disk, returning an empty cache on any error.
    pub fn load() -> Self {
        Self::try_load().unwrap_or_else(|| Self { servers: vec![] })
    }

    fn try_load() -> Option<Self> {
        let path = kmux_protocol::dirs::state_dir().ok()?.join(CACHE_FILE);
        let data = std::fs::read_to_string(path).ok()?;
        let servers: Vec<RecentServer> = serde_json::from_str(&data).ok()?;
        Some(Self { servers })
    }

    /// Persist the cache to disk without blocking the caller.
    ///
    /// Errors are silently ignored — cache loss is non-fatal.
    pub fn save(&self) {
        let path = match kmux_protocol::dirs::state_dir().map(|d| d.join(CACHE_FILE)) {
            Ok(p) => p,
            Err(e) => {
                tracing::trace!("Cache error {e:?}");
                return;
            }
        };
        let Ok(data) = serde_json::to_string_pretty(&self.servers) else {
            return;
        };
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::write(path, data);
        });
    }

    /// Record a successful connection. Upserts by `server_string`, bumps `last_used`,
    /// sorts by recency, and caps the list at `MAX_SERVERS`.
    pub fn record_connection(&mut self, server_string: &str, display: &str, kind: ServerKind) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(entry) = self
            .servers
            .iter_mut()
            .find(|s| s.server_string == server_string)
        {
            entry.last_used = now;
            entry.display = display.to_string();
            entry.kind = kind;
        } else {
            self.servers.push(RecentServer {
                server_string: server_string.to_string(),
                display: display.to_string(),
                last_used: now,
                sessions: vec![],
                kind,
            });
        }
        self.servers.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        self.servers.truncate(MAX_SERVERS);
        self.save();
    }

    /// Replace the cached session list for `server_string` with the live session list
    /// from the server (self-healing: sessions that no longer exist are dropped).
    pub fn update_sessions(&mut self, server_string: &str, live: &[SessionEntry]) {
        if let Some(entry) = self
            .servers
            .iter_mut()
            .find(|s| s.server_string == server_string)
        {
            entry.sessions = live
                .iter()
                .map(|s| CachedSession {
                    word_id: s.meta.word_id.clone(),
                    name: s.meta.name.clone(),
                    cwd: s.meta.cwd.clone(),
                })
                .collect();
            self.save();
        }
    }

    /// Read-only access to the cached server list.
    pub fn servers(&self) -> &[RecentServer] {
        &self.servers
    }
}
