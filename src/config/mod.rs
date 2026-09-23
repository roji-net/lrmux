// Config: TOML config loading + keybinding map.

use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub prefix: PrefixConfig,
    pub keys: std::collections::HashMap<String, String>,
    pub fkeys: FkeyConfig,
    pub statusbar: StatusbarConfig,
    pub colors: ColorsConfig,
    pub behavior: BehaviorConfig,
}

#[derive(Debug, Deserialize)]
pub struct PrefixConfig {
    pub key: String,
    pub double_send: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct FkeyConfig {
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct StatusbarConfig {
    pub enabled: bool,
    pub position: String,
    pub elements: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ColorsConfig {
    pub statusbar_fg: String,
    pub statusbar_bg: String,
    pub active_window_fg: String,
    pub active_window_bg: String,
    pub filler_bg: String,
    pub filler_border: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct BehaviorConfig {
    pub default_shell: String,
    pub scrollback_lines: usize,
    pub confirm_kill: bool,
    pub renumber_windows: bool,
    pub clipboard_cmd: String,
}

impl Default for PrefixConfig {
    fn default() -> Self {
        Self {
            key: "ctrl-a".to_string(),
            double_send: true,
        }
    }
}

pub fn load() -> Config {
    // Phase 5+: load ~/.config/lrmux/config.toml.
    Config::default()
}
