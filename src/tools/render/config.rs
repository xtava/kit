use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::framework::ConfigStore;

const TOOL: &str = "render";

#[derive(Clone, Debug)]
pub struct Config {
    store: ConfigStore,
    show_git_ignored: bool,
    theme: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    #[serde(default = "default_show_git_ignored")]
    show_git_ignored: bool,
    #[serde(default = "default_theme")]
    theme: String,
}

impl Default for Stored {
    fn default() -> Self {
        Self { show_git_ignored: default_show_git_ignored(), theme: default_theme() }
    }
}

impl Config {
    pub fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self { store, show_git_ignored: stored.show_git_ignored, theme: stored.theme })
    }

    pub fn show_git_ignored(&self) -> bool {
        self.show_git_ignored
    }

    pub fn set_show_git_ignored(&mut self, show: bool) -> Result<()> {
        self.store.save(TOOL, &Stored { show_git_ignored: show, theme: self.theme.clone() })?;
        self.show_git_ignored = show;
        Ok(())
    }

    pub fn theme(&self) -> &str {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: &str) -> Result<()> {
        self.store.save(
            TOOL,
            &Stored { show_git_ignored: self.show_git_ignored, theme: theme.to_owned() },
        )?;
        self.theme = theme.to_owned();
        Ok(())
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.store.path(TOOL)
    }

    pub fn theme_dir(&self) -> std::path::PathBuf {
        self.path().with_file_name("themes")
    }
}

fn default_show_git_ignored() -> bool {
    true
}

fn default_theme() -> String {
    "nord".to_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("kit-render-config-test-{}-{nonce}-{sequence}", std::process::id()))
    }

    #[test]
    fn defaults_to_showing_ignored_markdown() -> Result<()> {
        let dir = temp_dir();
        let config = Config::load(ConfigStore::rooted(dir.clone()))?;
        let _ = std::fs::remove_dir_all(dir);
        assert!(config.show_git_ignored());
        assert_eq!(config.theme(), "nord");
        Ok(())
    }

    #[test]
    fn persists_ignored_visibility() -> Result<()> {
        let dir = temp_dir();
        let store = ConfigStore::rooted(dir.clone());
        let mut config = Config::load(store.clone())?;
        config.set_show_git_ignored(false)?;
        config.set_theme("terminal")?;

        let reloaded = Config::load(store)?;
        let _ = std::fs::remove_dir_all(dir);
        assert!(!reloaded.show_git_ignored());
        assert_eq!(reloaded.theme(), "terminal");
        Ok(())
    }
}
