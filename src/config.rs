//! Optional TOML config shared by the `ccal` TUI and `ccal-server`.
//!
//! Precedence everywhere is: explicit env var > config file > built-in
//! default. Existing env-only deployments keep working untouched; the file
//! just replaces the long `CCAL_*` incantations for people who'd rather not
//! export them.
//!
//! Location: `$CCAL_CONFIG` if set, otherwise `<os-config-dir>/ccal/
//! config.toml` (`~/.config/ccal/config.toml` on Linux,
//! `~/Library/Application Support/ccal/config.toml` on macOS). A missing
//! file is **not** an error — it means "env / defaults only".
//!
//! ```toml
//! # Shared secret. Both roles fall back to this when their section omits
//! # `token`, so the common single-operator case is one line.
//! token = "a-long-random-string"
//!
//! [client]
//! url = "ws://host:8787/sync/ccal"
//!
//! [server]
//! addr = "0.0.0.0:8787"     # bind all interfaces; pick any port here
//! # data_dir = "/var/lib/ccal"   # optional; default is the OS data dir
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Built-in listen address when neither env nor file says otherwise.
/// Loopback by design: opening to the network is an explicit choice.
pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:8787";

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Shared bearer token; per-section `token` overrides this.
    pub token: Option<String>,
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub calendar: CalendarConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct CalendarConfig {
    /// How often (seconds) the TUI refetches each subscribed ICS feed.
    /// Default 300 (5 min); the `r` key forces an immediate refresh
    /// regardless.
    pub refresh_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ClientConfig {
    /// Full sync URL, e.g. `ws://host:8787/sync/ccal`.
    pub url: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServerConfig {
    /// `host:port` to bind. Use `0.0.0.0:PORT` to listen on all interfaces.
    pub addr: Option<String>,
    pub token: Option<String>,
    /// Directory for `{docid}.automerge` replicas.
    pub data_dir: Option<String>,
    /// Opt-in: expose the embedded MCP server at `/mcp` (same listener,
    /// same bearer token). Off unless explicitly enabled.
    pub mcp: Option<bool>,
    /// Which docid the MCP server reads/writes. Defaults to `ccal` — the
    /// same replica a TUI on `ws://…/sync/ccal` shares, so assistant edits
    /// propagate live through the existing change-broadcast.
    pub mcp_doc: Option<String>,
}

/// Resolved path of the config file (whether or not it exists).
pub fn path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("CCAL_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let dirs = directories::ProjectDirs::from("", "", "ccal")
        .context("could not determine a config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Load the config file, or an all-`None` default if it doesn't exist.
    /// A present-but-malformed file is a hard error — silently ignoring it
    /// would hide the operator's intent (e.g. a typo'd bind address).
    pub fn load() -> Result<Self> {
        let path = path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Client sync URL: `$CCAL_SYNC_URL` wins, else `[client] url`.
    /// Empty strings are treated as unset so a blank env var disables sync.
    pub fn client_url(&self) -> Option<String> {
        env_or("CCAL_SYNC_URL", self.client.url.as_deref())
    }

    /// Client token: `$CCAL_SYNC_TOKEN` > `[client] token` > top-level.
    pub fn client_token(&self) -> Option<String> {
        env_or(
            "CCAL_SYNC_TOKEN",
            self.client.token.as_deref().or(self.token.as_deref()),
        )
    }

    /// Server bind address, always resolved: `$CCAL_SYNC_ADDR` >
    /// `[server] addr` > [`DEFAULT_SERVER_ADDR`].
    pub fn server_addr(&self) -> String {
        env_or("CCAL_SYNC_ADDR", self.server.addr.as_deref())
            .unwrap_or_else(|| DEFAULT_SERVER_ADDR.to_string())
    }

    /// Server token: `$CCAL_SYNC_TOKEN` > `[server] token` > top-level.
    pub fn server_token(&self) -> Option<String> {
        env_or(
            "CCAL_SYNC_TOKEN",
            self.server.token.as_deref().or(self.token.as_deref()),
        )
    }

    /// Server data dir: `$CCAL_SYNC_DATA` > `[server] data_dir` > `None`
    /// (caller falls back to the OS data dir).
    pub fn server_data_dir(&self) -> Option<PathBuf> {
        env_or("CCAL_SYNC_DATA", self.server.data_dir.as_deref()).map(PathBuf::from)
    }

    /// Whether to expose the embedded MCP server: `$CCAL_MCP` (truthy:
    /// `1`/`true`/`yes`/`on`, case-insensitive) > `[server] mcp` > `false`.
    /// Off by design — the MCP surface is full read+write over whatever
    /// guards the listener, so turning it on is an explicit choice.
    pub fn server_mcp_enabled(&self) -> bool {
        match std::env::var("CCAL_MCP") {
            Ok(v) => matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ),
            Err(_) => self.server.mcp.unwrap_or(false),
        }
    }

    /// Calendar refresh interval in seconds: `$CCAL_CAL_REFRESH` >
    /// `[calendar] refresh_secs` > `300`. Clamped to a sane floor so a
    /// typo can't hammer providers.
    pub fn calendar_refresh_secs(&self) -> u64 {
        let v = match std::env::var("CCAL_CAL_REFRESH") {
            Ok(s) => s.trim().parse::<u64>().ok(),
            Err(_) => None,
        }
        .or(self.calendar.refresh_secs)
        .unwrap_or(300);
        v.max(30)
    }

    /// Docid the MCP server operates on: `$CCAL_MCP_DOC` >
    /// `[server] mcp_doc` > `ccal` (the conventional default docid).
    pub fn server_mcp_doc(&self) -> String {
        env_or("CCAL_MCP_DOC", self.server.mcp_doc.as_deref())
            .unwrap_or_else(|| "ccal".to_string())
    }
}

/// Env var if set and non-empty, else the file value if non-empty.
fn env_or(var: &str, file: Option<&str>) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => return Some(v),
        _ => {}
    }
    file.filter(|v| !v.trim().is_empty()).map(str::to_string)
}
