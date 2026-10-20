//! Configuration loading for muxmesh (shareable config.toml + separate secret token).
//!
//! Atuin-like convention:
//!   - config.toml = shareable (bind, state_path, ttl, hub host...). Committed as a template.
//!   - token       = secret, mode 0600, NEVER in config.toml, read from its own file.
//!
//! Precedence everywhere: env > config.toml > default.
//! std-only (no toml dependency): minimal key=value parser for our flat sections.

use std::fs;
use std::path::PathBuf;

/// Hub configuration: listen address + state file.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub bind: String,
    pub state_path: PathBuf,
    /// Hub address the poller reads live state from ("host:port"). On a
    /// driver machine there is no local state.json — the hub is the source.
    /// Defaults to hub.bind; set poll.hub for a remote hub (e.g. the LAN IP
    /// of the pivot). Env: MUXMESH_POLL_HUB.
    pub poll_hub: String,
    pub ttl: u64,
    pub interval: u64,
    /// Path of the per-machine token registry (tokens.json, digests only).
    pub tokens_path: PathBuf,
}

/// Agent configuration (remote machine).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub hub: String,
    pub port: u16,
    pub host: String,
    pub interval: u64,
    /// local HTTP port where the agent serves POST /cmd (0 => cmd disabled).
    pub cmd_port: u16,
    /// local tmux socket the agent controls (empty => tmux default).
    pub tmux_socket: String,
}

/// Shared token between hub and agents. Never in config.toml.
/// Debug is redacted so the secret can never leak through `{:?}`.
#[derive(Clone)]
pub struct Token(pub(crate) String);

impl Token {
    /// The raw secret, for signing requests / comparing at the boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

/// Config directory: ~/.config/muxmesh (override with MUXMESH_CONFIG_DIR).
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("MUXMESH_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("muxmesh")
}

/// Reads a secret file (explicit path > $MUXMESH_TOKEN_PATH > <config_dir>/token).
/// Returns Token("") when absent (the hub then rejects every POST -> no token = closed).
pub fn read_token() -> Token {
    read_secret_file("MUXMESH_TOKEN_PATH", "token")
}

/// Generic secret reader: env path override > <config_dir>/<name>. Missing
/// file => empty (deny). Other errors (EACCES, EISDIR) are operator mistakes
/// -> say so loudly, then deny.
pub(crate) fn read_secret_file(env: &str, name: &str) -> Token {
    let path = std::env::var(env)
        .map(PathBuf::from)
        .unwrap_or_else(|_| config_dir().join(name));
    match fs::read_to_string(&path) {
        Ok(s) => Token(s.trim().to_string()),
        // Absent file: silent deny (fresh install). Anything else (EACCES,
        // EISDIR, wrong owner) is an operator mistake -> say so loudly.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Token(String::new()),
        Err(e) => {
            eprintln!(
                "muxmesh: cannot read token {}: {e} (treating as absent -> deny)",
                path.display()
            );
            Token(String::new())
        }
    }
}

/// Minimal TOML parse: reads only `key = value` (and "value"), skipping sections/comments.
/// Enough for our flat [hub]/[poll]/[agent] tables. Not a full TOML parser.
/// Returns a map "section.key" -> raw value (quotes stripped).
pub fn parse_flat_toml(content: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut section = String::new();
    for raw in content.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.trim().to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = format!("{section}.{}", k.trim());
            let val = v.trim().trim_matches('"').to_string();
            map.insert(key, val);
        }
    }
    map
}

/// Loads config.toml (if present) and applies precedence env > toml > default.
fn load_map() -> std::collections::HashMap<String, String> {
    let path = config_dir().join("config.toml");
    match fs::read_to_string(&path) {
        Ok(c) => parse_flat_toml(&c),
        Err(_) => std::collections::HashMap::new(),
    }
}

/// env > toml(section.key) > default.
fn pick(
    env: &str,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    default: &str,
) -> String {
    if let Ok(v) = std::env::var(env) {
        return v;
    }
    map.get(key).cloned().unwrap_or_else(|| default.to_string())
}

/// "host" or "host:port" -> "host:port". Bare IPv6 ("::1") is bracketed so
/// the appended port stays parseable; bracketed input passes through.
/// Split "host:port" (v6 bracketed or bare v4/host) into its parts.
fn split_host_port(addr: &str) -> (&str, Option<&str>) {
    let a = addr.trim();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let p = tail.strip_prefix(':').filter(|p| !p.is_empty());
            return (h, p);
        }
        return (a, None);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, Some(p)),
        // bare IPv6 (multiple colons) or bare host -> no port part
        _ => (a, None),
    }
}

fn normalize_addr(host: &str, default_port: &str) -> String {
    let h = host.trim();
    if h.starts_with('[') {
        // bracketed: keep only if a port follows "]"; append the default otherwise
        return match split_host_port(h) {
            (_, Some(_)) => h.to_string(),
            (h6, None) => format!("[{h6}]:{default_port}"),
        };
    }
    let colons = h.chars().filter(|c| *c == ':').count();
    if colons == 0 {
        format!("{h}:{default_port}")
    } else if colons == 1 {
        h.to_string()
    } else {
        // bare IPv6 -> bracket + port
        format!("[{h}]:{default_port}")
    }
}

/// Poll-hub address for a process configured through [agent] hub/port (the
/// agent's embedded poller). Explicit poll.hub / MUXMESH_POLL_HUB wins;
/// otherwise the agent's own push target, so a remote agent never badges
/// against loopback by accident.
pub fn agent_poll_hub(agent: &AgentConfig) -> String {
    let explicit = std::env::var("MUXMESH_POLL_HUB")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            load_map()
                .get("poll.hub")
                .cloned()
                .filter(|s| !s.trim().is_empty())
        });
    normalize_addr(
        explicit.as_deref().unwrap_or(&agent.hub),
        &agent.port.to_string(),
    )
}

impl HubConfig {
    pub fn load() -> Self {
        let m = load_map();
        let bind = pick("MUXMESH_BIND", &m, "hub.bind", "0.0.0.0:9997");
        let state_path = expand_tilde(&pick(
            "MUXMESH_STATE",
            &m,
            "hub.state_path",
            "~/.muxmesh/state.json",
        ));
        let ttl = pick("MUXMESH_TTL", &m, "poll.ttl", "30")
            .parse()
            .unwrap_or(30);
        let interval = pick("MUXMESH_INTERVAL", &m, "poll.interval", "2")
            .parse()
            .unwrap_or(2);
        let tokens_path = expand_tilde(&pick(
            "MUXMESH_TOKENS",
            &m,
            "hub.tokens_path",
            "~/.muxmesh/tokens.json",
        ));
        // poll.hub: where the poller fetches state ("host" or "host:port").
        // Default = bind host so a hub-local poller needs no config; a driver
        // sets it to the hub's LAN IP. Bare IPv6 gets bracketed at use time.
        let (bind_host, bind_port) = split_host_port(&bind);
        let bind_host = if bind_host == "0.0.0.0" || bind_host.is_empty() {
            "127.0.0.1"
        } else {
            bind_host
        };
        let bind_port = bind_port.unwrap_or("9997");
        let poll_hub_raw = pick("MUXMESH_POLL_HUB", &m, "poll.hub", bind_host);
        let poll_hub = normalize_addr(&poll_hub_raw, bind_port);
        Self {
            bind,
            state_path,
            poll_hub,
            ttl,
            interval,
            tokens_path,
        }
    }
}

impl AgentConfig {
    pub fn load() -> Self {
        let m = load_map();
        let hub = pick("MUXMESH_HUB", &m, "agent.hub", "127.0.0.1");
        let port = pick("MUXMESH_PORT", &m, "agent.port", "9997")
            .parse()
            .unwrap_or(9997);
        let mut host = pick("MUXMESH_HOST", &m, "agent.host", "");
        if host.is_empty() {
            host = hostname_short();
        }
        let interval = pick("MUXMESH_INTERVAL", &m, "agent.interval", "15")
            .parse()
            .unwrap_or(15);
        let cmd_port = pick("MUXMESH_CMD_PORT", &m, "agent.cmd_port", "9998")
            .parse()
            .unwrap_or(9998);
        let tmux_socket = pick("MUXMESH_TMUX_SOCKET", &m, "agent.tmux_socket", "");
        Self {
            hub,
            port,
            host,
            interval,
            cmd_port,
            tmux_socket,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}/state", self.hub, self.port)
    }
}

fn hostname_short() -> String {
    let out = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok();
    match out {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Expands a leading ~ using $HOME.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Constant-time comparison (anti timing-attack on the token).
pub fn token_matches(expected: &str, got: &str) -> bool {
    if expected.is_empty() {
        return false; // no token configured => reject everything (safe default)
    }
    let a = expected.as_bytes();
    let b = got.as_bytes();
    if a.len() != b.len() {
        return false; // length leaks via size; inherent and acceptable
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_addr_host_v4_ipv6() {
        assert_eq!(normalize_addr("10.0.10.3", "9997"), "10.0.10.3:9997");
        assert_eq!(normalize_addr("10.0.10.3:9998", "9997"), "10.0.10.3:9998");
        assert_eq!(normalize_addr("::1", "9997"), "[::1]:9997");
        assert_eq!(normalize_addr("[::1]:9998", "9997"), "[::1]:9998");
    }

    #[test]
    fn normalize_addr_bracketed_v6_without_port() {
        assert_eq!(normalize_addr("[::1]", "9997"), "[::1]:9997");
        assert_eq!(normalize_addr("[::]:9998", "9997"), "[::]:9998");
    }

    #[test]
    fn split_host_port_v6_bracketed_keeps_port() {
        assert_eq!(split_host_port("[::1]:9997"), ("::1", Some("9997")));
        assert_eq!(split_host_port("[::1]"), ("::1", None));
        assert_eq!(
            split_host_port("10.0.10.3:9997"),
            ("10.0.10.3", Some("9997"))
        );
        assert_eq!(split_host_port("10.0.10.3"), ("10.0.10.3", None));
    }

    #[test]
    fn parse_flat_sections_and_keys() {
        let toml = "[hub]\nbind = \"0.0.0.0:9997\"\n[poll]\nttl = 30\n# comment\n";
        let m = parse_flat_toml(toml);
        assert_eq!(m.get("hub.bind").map(|s| s.as_str()), Some("0.0.0.0:9997"));
        assert_eq!(m.get("poll.ttl").map(|s| s.as_str()), Some("30"));
    }

    #[test]
    fn env_overrides_toml() {
        std::env::set_var("MUXMESH_TTL", "5");
        let mut m = std::collections::HashMap::new();
        m.insert("poll.ttl".into(), "30".into());
        assert_eq!(pick("MUXMESH_TTL", &m, "poll.ttl", "30"), "5");
        std::env::remove_var("MUXMESH_TTL");
    }

    #[test]
    fn token_matches_constant_time_semantics() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc123", "abc124"));
        assert!(!token_matches("abc123", "abc"));
        assert!(!token_matches("", "")); // empty expected => closed
    }

    #[test]
    fn expand_tilde_uses_home() {
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(
            expand_tilde("~/.muxmesh/state.json"),
            PathBuf::from("/home/tester/.muxmesh/state.json")
        );
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn agent_url_points_at_hub() {
        let cfg = AgentConfig {
            hub: "10.0.0.1".into(),
            port: 9997,
            host: "a".into(),
            interval: 15,
            cmd_port: 9998,
            tmux_socket: String::new(),
        };
        assert_eq!(cfg.url(), "http://10.0.0.1:9997/state");
    }

    #[test]
    fn hub_defaults_are_loopback_capable() {
        // no config file present in CI: defaults must load without panicking
        let cfg = HubConfig::load();
        assert!(cfg.bind.ends_with(":9997"));
        assert!(cfg.ttl > 0);
    }

    #[test]
    fn token_debug_never_prints_the_secret() {
        let t = Token("super-secret-value".into());
        assert_eq!(format!("{t:?}"), "Token(***)");
    }

    #[test]
    fn flat_toml_rejects_invalid_names_via_valid_name() {
        use crate::valid_name;
        // names that would poison tmux targets downstream
        assert!(!valid_name("host;run-shell x"));
        assert!(!valid_name("sess:0.1"));
        assert!(valid_name("host-a_1"));
    }
}
