// Config: TOML config loading + keybinding map + network settings.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub prefix: PrefixConfig,
    #[serde(default)]
    pub keys: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub fkeys: FkeyConfig,
    #[serde(default)]
    pub statusbar: StatusbarConfig,
    #[serde(default)]
    pub colors: ColorsConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PrefixConfig {
    #[serde(default = "default_prefix_key")]
    pub key: String,
    #[serde(default = "default_true")]
    pub double_send: bool,
}

fn default_prefix_key() -> String {
    "ctrl-a".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct FkeyConfig {
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct StatusbarConfig {
    pub enabled: bool,
    #[serde(default)]
    pub position: String,
    #[serde(default)]
    pub elements: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ColorsConfig {
    #[serde(default)]
    pub statusbar_fg: String,
    #[serde(default)]
    pub statusbar_bg: String,
    #[serde(default)]
    pub active_window_fg: String,
    #[serde(default)]
    pub active_window_bg: String,
    #[serde(default)]
    pub filler_bg: String,
    #[serde(default)]
    pub filler_border: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct BehaviorConfig {
    #[serde(default)]
    pub default_shell: String,
    #[serde(default)]
    pub scrollback_lines: usize,
    pub confirm_kill: bool,
    pub renumber_windows: bool,
    #[serde(default)]
    pub clipboard_cmd: String,
}

impl Default for PrefixConfig {
    fn default() -> Self {
        Self {
            key: default_prefix_key(),
            double_send: true,
        }
    }
}

/// TLS mode for network connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    /// Never use TLS (insecure; only for explicitly trusted networks).
    Off,
    /// Always require TLS.
    On,
    /// Require TLS unless the peer IP falls in `safe_networks`.
    /// With an empty `safe_networks` list (the default), this equals `On`.
    #[default]
    Auto,
}

impl<'de> Deserialize<'de> for TlsMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "no" => Ok(TlsMode::Off),
            "on" | "true" | "1" | "yes" => Ok(TlsMode::On),
            "auto" | "" => Ok(TlsMode::Auto),
            other => Err(serde::de::Error::custom(format!(
                "invalid tls mode '{other}', expected off|on|auto"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct NetworkConfig {
    /// TCP listen address, e.g. "0.0.0.0:17280". Empty = no TCP listener.
    #[serde(default)]
    pub tcp_listen: String,
    /// Respond to UDP discovery probes.
    #[serde(default)]
    pub discovery: bool,
    /// UDP discovery port (clients broadcast here; servers listen).
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,
    /// TLS policy for TCP connections.
    #[serde(default)]
    pub tls: TlsMode,
    /// Pre-shared key required on TCP Identify when non-empty.
    /// Preferred name; `auth_token` is accepted as a deprecated alias.
    #[serde(default)]
    pub psk: String,
    /// Deprecated alias for `psk` (still read for backward compatibility).
    #[serde(default)]
    pub auth_token: String,
    /// CIDR list where plaintext is allowed when `tls = "auto"`.
    /// Empty by default (safe): every peer requires TLS under `auto`.
    /// Example for a classic home/office LAN:
    /// `safe_networks = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "127.0.0.0/8"]`
    #[serde(default)]
    pub safe_networks: Vec<String>,
    /// Optional PEM certificate path (default: ~/.config/lrmux/certs/server.crt).
    #[serde(default)]
    pub tls_cert: String,
    /// Optional PEM private key path (default: ~/.config/lrmux/certs/server.key).
    #[serde(default)]
    pub tls_key: String,
    /// WebSocket listen address for browser clients, e.g. "127.0.0.1:17282".
    /// Empty = disabled. Same binary protocol as TCP, carried in WS binary frames.
    /// For remote/HTTPS pages put a reverse proxy in front for WSS (Caddy/nginx).
    #[serde(default)]
    pub ws_listen: String,
}

fn default_discovery_port() -> u16 {
    17280
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            tcp_listen: String::new(),
            discovery: false,
            discovery_port: default_discovery_port(),
            tls: TlsMode::Auto,
            psk: String::new(),
            auth_token: String::new(),
            safe_networks: Vec::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            ws_listen: String::new(),
        }
    }
}

impl NetworkConfig {
    pub fn tcp_listen_addr(&self) -> Option<&str> {
        let s = self.tcp_listen.trim();
        if s.is_empty() { None } else { Some(s) }
    }

    pub fn ws_listen_addr(&self) -> Option<&str> {
        let s = self.ws_listen.trim();
        if s.is_empty() { None } else { Some(s) }
    }

    /// Effective PSK (`psk`, falling back to deprecated `auth_token`).
    pub fn psk_value(&self) -> &str {
        if !self.psk.is_empty() {
            &self.psk
        } else {
            &self.auth_token
        }
    }

    pub fn has_psk(&self) -> bool {
        !self.psk_value().is_empty()
    }
}

/// Directory for user config: `~/.config/lrmux`.
pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("lrmux");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("lrmux")
}

/// Path to the main config file.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Default directory for auto-generated TLS certs.
pub fn certs_dir() -> PathBuf {
    config_dir().join("certs")
}

/// Load config from `~/.config/lrmux/config.toml` (or `$XDG_CONFIG_HOME/lrmux`).
/// Missing file → defaults (network disabled). Invalid TOML → defaults + stderr warning.
pub fn load() -> Config {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(contents) => match toml::from_str::<Config>(&contents) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("lrmux: warning: failed to parse {}: {e}", path.display());
                Config::default()
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            eprintln!("lrmux: warning: failed to read {}: {e}", path.display());
            Config::default()
        }
    }
}

/// Process-wide config snapshot, loaded once on first access.
static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();

/// Optional CLI / in-session override for the client PSK (`--psk` / `LRMUX_PSK`).
static PSK_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

pub fn global() -> &'static Config {
    CONFIG.get_or_init(load)
}

/// Override the global config (tests / early CLI). Call before `global()`.
#[allow(clippy::result_large_err)]
pub fn set_global(cfg: Config) -> Result<(), Config> {
    CONFIG.set(cfg)
}

/// Set a process-local PSK override (does not rewrite the config file).
pub fn set_psk_override(psk: Option<String>) {
    *PSK_OVERRIDE.lock().unwrap() = psk;
}

/// Resolve the PSK the client should present: override → env → config.
pub fn effective_psk() -> String {
    if let Some(p) = PSK_OVERRIDE.lock().unwrap().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("LRMUX_PSK")
        && !p.is_empty()
    {
        return p;
    }
    global().network.psk_value().to_string()
}

/// Persist `network.psk` (and drop deprecated `auth_token`) into config.toml.
/// Creates the file / `[network]` section as needed. Also updates the in-memory
/// override so the current process uses the new value immediately.
pub fn persist_psk(psk: &str) -> io::Result<()> {
    set_psk_override(Some(psk.to_string()));
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let mut root: toml::Value = match fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Value::Table(toml::map::Map::new()),
        Err(e) => return Err(e),
    };

    let table = root
        .as_table_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config root is not a table"))?;
    let network = table
        .entry("network".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let net = network
        .as_table_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "[network] is not a table"))?;
    net.insert("psk".to_string(), toml::Value::String(psk.to_string()));
    net.remove("auth_token");

    let serialized = toml::to_string_pretty(&root)
        .map_err(|e| io::Error::other(format!("serialize config: {e}")))?;
    fs::write(&path, serialized)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Generate a URL-safe random PSK (~160 bits of entropy).
pub fn generate_psk() -> io::Result<String> {
    let mut buf = [0u8; 20];
    fill_random(&mut buf)?;
    Ok(base64url(&buf))
}

fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    let mut f = fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

fn base64url(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
    }
    out
}
