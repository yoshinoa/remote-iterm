use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_SERVER_URL: &str = "https://term.jiye.ca:8443";

fn default_server_url() -> String {
    DEFAULT_SERVER_URL.to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    #[serde(default = "default_server_url")]
    pub server_url: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub window: Option<SavedWindow>,
    #[serde(default)]
    pub font_size: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self { server_url: default_server_url(), key: None, window: None, font_size: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SavedWindow {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("remote-iterm")
        .join("desktop.json")
}

pub fn load(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

pub fn remember_window(path: &Path, window: SavedWindow) -> std::io::Result<()> {
    let mut cfg = load(path);
    cfg.window = Some(window);
    save(path, &cfg)
}

pub fn remember_font_size(path: &Path, size: f32) -> std::io::Result<()> {
    let mut cfg = load(path);
    cfg.font_size = Some(size);
    save(path, &cfg)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_key_input(input: &str) -> Option<String> {
    let input = input.trim();
    let key = match input.split_once('#') {
        Some((_, fragment)) => fragment
            .split('&')
            .find_map(|pair| pair.strip_prefix("key="))
            .map(percent_decode)?,
        None => input.to_string(),
    };
    let key = key.trim().to_string();
    (!key.is_empty()).then_some(key)
}

pub fn with_env_overrides(mut cfg: Config) -> Config {
    if let Ok(url) = std::env::var("REMOTE_ITERM_URL") {
        cfg.server_url = url;
    }
    if let Ok(key) = std::env::var("REMOTE_ITERM_KEY") {
        cfg.key = Some(key);
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("remote-iterm-desktop-test-{}-{name}", std::process::id()))
            .join("desktop.json")
    }

    #[test]
    fn parses_raw_key() {
        assert_eq!(parse_key_input("  abc123  ").as_deref(), Some("abc123"));
    }

    #[test]
    fn parses_qr_link() {
        assert_eq!(parse_key_input("https://h:7292/#key=ab%2Fc").as_deref(), Some("ab/c"));
        assert_eq!(parse_key_input("http://10.0.0.2:7292/#x=1&key=k9").as_deref(), Some("k9"));
        assert_eq!(parse_key_input("https://h/#key=a%é").as_deref(), Some("a%é"));
    }

    #[test]
    fn rejects_blank() {
        assert_eq!(parse_key_input("   "), None);
        assert_eq!(parse_key_input("https://h/#key="), None);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let cfg = load(&temp_path("missing"));
        assert_eq!(cfg.server_url, DEFAULT_SERVER_URL);
        assert_eq!(cfg.key, None);
        assert_eq!(cfg.window, None);
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp_path("roundtrip");
        let cfg = Config {
            server_url: "https://example:8443".into(),
            key: Some("secret".into()),
            window: Some(SavedWindow { x: 10.0, y: 20.0, width: 900.0, height: 600.0, maximized: true }),
            font_size: Some(16.0),
        };
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path), cfg);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn remember_window_keeps_the_file_key() {
        let path = temp_path("remember");
        save(&path, &Config { key: Some("filekey".into()), ..Config::default() }).unwrap();
        let w = SavedWindow { x: 1.0, y: 2.0, width: 800.0, height: 500.0, maximized: false };
        remember_window(&path, w).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.key.as_deref(), Some("filekey"));
        assert_eq!(loaded.window, Some(w));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_overrides_apply() {
        unsafe {
            std::env::set_var("REMOTE_ITERM_URL", "http://localhost:7291");
            std::env::set_var("REMOTE_ITERM_KEY", "devkey");
        }
        let cfg = with_env_overrides(Config::default());
        unsafe {
            std::env::remove_var("REMOTE_ITERM_URL");
            std::env::remove_var("REMOTE_ITERM_KEY");
        }
        assert_eq!(cfg.server_url, "http://localhost:7291");
        assert_eq!(cfg.key.as_deref(), Some("devkey"));
    }
}
