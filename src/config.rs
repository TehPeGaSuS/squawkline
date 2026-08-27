use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub nickname: String,
    pub server: String,
    pub port: Option<u16>,
    pub use_tls: bool,
    pub channels: Vec<String>,
    // SASL PLAIN. Omit both (or leave sasl_password empty) to skip SASL —
    // it's only attempted if the server actually offers the "sasl" cap and
    // a password is set. account defaults to `nickname` when unset, since
    // that's the common case (nick == account name).
    #[serde(default)]
    pub sasl_account: Option<String>,
    #[serde(default)]
    pub sasl_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub servers: Vec<ServerConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            servers: vec![
                ServerConfig {
                    name: "libera".to_owned(),
                    nickname: "squawkline".to_owned(),
                    server: "irc.libera.chat".to_owned(),
                    port: None,
                    use_tls: true,
                    channels: vec!["##squawkline-test".to_owned()],
                    sasl_account: None,
                    sasl_password: None,
                },
            ],
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/squawkline/config.toml"))
}

/// Loads config from `~/.config/squawkline/config.toml`, writing out
/// defaults on first run so the file exists for the user to edit. Add more
/// entries under `[[servers]]` to connect to more than one network.
pub fn load_or_init() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };

    if let Ok(raw) = std::fs::read_to_string(&path) {
        match toml::from_str(&raw) {
            Ok(cfg) => return cfg,
            Err(e) => eprintln!("warning: {} is invalid ({e}), using defaults", path.display()),
        }
    } else {
        let default = AppConfig::default();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(toml_str) = toml::to_string_pretty(&default) {
            let _ = std::fs::write(&path, toml_str);
        }
        return default;
    }

    AppConfig::default()
}
