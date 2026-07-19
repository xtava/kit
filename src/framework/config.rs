use std::path::PathBuf;

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{de::DeserializeOwned, Serialize};
use toml_edit::{value as toml_value, DocumentMut};

use super::AtomicFileWriter;

/// Scalar value written losslessly into a tool's TOML Settings document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Integer(i64),
    String(String),
}

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
        let raw = toml::to_string_pretty(value).context("serialize config")?;
        let path = self.path(tool);
        let writer = self.writer(tool);
        let _lock = writer.lock()?;
        writer.replace(&path, raw.as_bytes())?;
        Ok(())
    }

    /// Update one scalar while preserving unrelated TOML, comments, spacing, and item order.
    pub fn set(&self, tool: &str, key: &str, value: ConfigValue) -> Result<()> {
        let writer = self.writer(tool);
        let _lock = writer.lock()?;
        let path = self.path(tool);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let mut document =
            raw.parse::<DocumentMut>().with_context(|| format!("parse {}", path.display()))?;
        let item = match value {
            ConfigValue::Bool(value) => toml_value(value),
            ConfigValue::Integer(value) => toml_value(value),
            ConfigValue::String(value) => toml_value(value),
        };
        let decor =
            document.get(key).and_then(|item| item.as_value()).map(|value| value.decor().clone());
        document[key] = item;
        if let (Some(decor), Some(value)) =
            (decor, document.get_mut(key).and_then(|item| item.as_value_mut()))
        {
            *value.decor_mut() = decor;
        }
        writer.replace(&path, document.to_string().as_bytes())?;
        Ok(())
    }

    fn writer(&self, tool: &str) -> AtomicFileWriter {
        AtomicFileWriter::new(&self.dir, format!(".{tool}.lock"), format!(".{tool}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> ConfigStore {
        ConfigStore::rooted(
            std::env::temp_dir().join(format!("kit-config-{name}-{}", std::process::id())),
        )
    }

    #[test]
    fn scalar_edit_preserves_document_formatting() -> Result<()> {
        let store = store("preserve");
        std::fs::create_dir_all(&store.dir)?;
        let path = store.path("diff");
        std::fs::write(&path, "# review\nline_numbers = 'auto' # policy\nextra = 7\n")?;

        store.set("diff", "line_numbers", ConfigValue::String("never".to_owned()))?;
        let raw = std::fs::read_to_string(&path)?;

        assert!(raw.contains("# review"));
        assert!(raw.contains("# policy"));
        assert!(raw.contains("extra = 7"));
        assert!(raw.contains("line_numbers = \"never\""));
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn integer_edit_preserves_other_fields() -> Result<()> {
        let store = store("integer");
        std::fs::create_dir_all(&store.dir)?;
        let path = store.path("tail");
        std::fs::write(&path, "mouse = true\nsplit_ratio = 440\n")?;

        store.set("tail", "split_ratio", ConfigValue::Integer(615))?;
        let raw = std::fs::read_to_string(&path)?;

        assert!(raw.contains("mouse = true"));
        assert!(raw.contains("split_ratio = 615"));
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn scalar_edit_never_replaces_invalid_toml() -> Result<()> {
        let store = store("invalid");
        std::fs::create_dir_all(&store.dir)?;
        let path = store.path("diff");
        let invalid = "line_numbers = [\n";
        std::fs::write(&path, invalid)?;

        let error = store
            .set("diff", "line_numbers", ConfigValue::String("never".to_owned()))
            .expect_err("invalid TOML must block editing");

        assert!(format!("{error:#}").contains("parse"));
        assert_eq!(std::fs::read_to_string(&path)?, invalid);
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }
}
