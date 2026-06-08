use std::path::PathBuf;

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{de::DeserializeOwned, Serialize};

/// Per-tool persistent config, one TOML file per tool under the XDG config dir.
///
/// A missing file is a defined contract — it yields `T::default()`, not an error. A file that
/// exists but won't parse *is* an error: we never silently discard a config the user wrote.
#[derive(Clone, Debug)]
pub struct ConfigStore {
    dir: PathBuf,
}

impl ConfigStore {
    pub fn bootstrap() -> Result<Self> {
        let dir = ProjectDirs::from("", "", "kit")
            .context("resolve XDG config directory")?
            .config_dir()
            .to_path_buf();
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub(crate) fn rooted(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self, tool: &str) -> PathBuf {
        self.dir.join(format!("{tool}.toml"))
    }

    pub fn load<T: DeserializeOwned + Default>(&self, tool: &str) -> Result<T> {
        let path = self.path(tool);
        match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str(&raw).with_context(|| format!("parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save<T: Serialize>(&self, tool: &str, value: &T) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        let raw = toml::to_string_pretty(value).context("serialize config")?;
        let path = self.path(tool);
        std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))
    }
}
