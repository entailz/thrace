/*
SPDX-License-Identifier: AGPL-3.0-only
*/

//! Local client preferences, separate from Matrix sessions and account settings.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Preferences saved in config.toml. Missing fields use the client defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub font_size: f32,
    pub show_previews: bool,
    pub embeds: Embeds,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            font_size: 14.0,
            show_previews: true,
            embeds: Embeds::default(),
        }
    }
}

/// Embed providers and their hostname aliases, grouped under [embeds].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Embeds {
    pub rules: Vec<crate::embed::EmbedRule>,
}

impl Default for Embeds {
    fn default() -> Self {
        Self {
            rules: crate::embed::default_rules(),
        }
    }
}

impl Config {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            ["dark", "midnight", "bbs-amber", "win98"].contains(&self.theme.as_str()),
            "unknown theme: {}",
            self.theme
        );
        anyhow::ensure!(
            self.font_size.is_finite() && (10.0..=24.0).contains(&self.font_size),
            "font_size must be between 10 and 24"
        );
        Ok(())
    }
}

/// Config location and last successfully saved preferences.
pub struct ConfigFile {
    path: PathBuf,
    pub saved: Config,
}

/// Resolve the XDG config directory, falling back to $HOME/.config.
pub fn config_path() -> Result<PathBuf> {
    resolve_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn resolve_path(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    let base = xdg.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = match base {
        Some(base) => base,
        None => {
            PathBuf::from(home.context("HOME is unset; set XDG_CONFIG_HOME to an absolute path")?)
                .join(".config")
        }
    };
    Ok(base.join("thrace/config.toml"))
}

impl ConfigFile {
    /// Load preferences, creating the file on first launch. Invalid files are left intact.
    pub fn open(path: PathBuf) -> Result<Self> {
        let saved = match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str::<Config>(&raw)
                .with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let defaults = Config::default();
                write_config(&path, &defaults)?;
                defaults
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        saved
            .validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(Self { path, saved })
    }

    /// Write changed preferences without truncating the previous file on failure.
    pub fn save(&mut self, config: Config) -> Result<()> {
        if config != self.saved {
            config.validate()?;
            write_config(&self.path, &config)?;
            self.saved = config;
        }
        Ok(())
    }
}

fn write_config(path: &Path, config: &Config) -> Result<()> {
    let parent = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".config-{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        std::fs::write(&temp, toml::to_string_pretty(config)?)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("save {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_selectable_themes_load_timeline_colors() {
        for name in ["dark", "midnight", "bbs-amber", "win98"] {
            let config = Config {
                theme: name.into(),
                ..Config::default()
            };
            config.validate().unwrap();
            let theme = crate::theme::ThemeFile::load_builtin(name).unwrap();
            assert!(!theme.timeline.nick_colors.is_empty());
            assert!(theme.timeline.timestamp.is_some());
        }
    }

    #[test]
    fn xdg_paths_require_an_absolute_directory() {
        let home = Some("/home/test".into());
        assert_eq!(
            resolve_path(Some("/custom".into()), home.clone()).unwrap(),
            PathBuf::from("/custom/thrace/config.toml")
        );
        for xdg in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                resolve_path(xdg, home.clone()).unwrap(),
                PathBuf::from("/home/test/.config/thrace/config.toml")
            );
        }
        assert!(resolve_path(None, None).is_err());
    }

    #[test]
    fn partial_configs_preserve_defaults_and_validate_values() {
        let config: Config = toml::from_str("theme = 'midnight'").unwrap();
        assert_eq!(config.font_size, 14.0);
        assert!(!config.embeds.rules[0].enabled);
        assert!(config.validate().is_ok());
        for raw in ["font_size = nan", "font_size = 25", "theme = 'missing'"] {
            assert!(toml::from_str::<Config>(raw).unwrap().validate().is_err());
        }
    }

    #[test]
    fn preferences_survive_restart_and_invalid_files_are_preserved() {
        let dir = std::env::temp_dir().join(format!(
            "thrace-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");
        let mut file = ConfigFile::open(path.clone()).unwrap();
        let mut config = file.saved.clone();
        config.theme = "win98".into();
        config.font_size = 18.0;
        config.show_previews = false;
        config.embeds.rules[0].enabled = true;
        config.embeds.rules[0].open_with = "preview.example".into();
        config.embeds.rules[0].api = "https://preview.example/api".into();
        config.embeds.rules[0].hosts.push("example.com".into());
        let mut second = config.embeds.rules[0].clone();
        second.name = "Another provider".into();
        second.hosts = vec!["another.example".into()];
        config.embeds.rules.push(second);
        file.save(config.clone()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[[embeds.rules]]"));
        assert!(!raw.contains("[[embed_rules]]"));
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["embeds"]["rules"].as_array().unwrap().len(), 2);
        assert!(parsed["embeds"]["rules"][0]["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|host| host.as_str() == Some("example.com")));
        assert_eq!(ConfigFile::open(path.clone()).unwrap().saved, config);
        std::fs::write(&path, "not valid toml").unwrap();
        assert!(ConfigFile::open(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not valid toml");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
