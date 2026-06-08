use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};

use super::engine::{canonicalize_query_token, canonicalize_suffix};
use super::DEFAULT_TLDS;
use crate::framework::ConfigStore;

const TOOL: &str = "domain";

/// The domain tool's live config — the active TLD set and saved favorites — backed by the
/// framework [`ConfigStore`]. Mutations (`set_tlds`, `add_favorite`) persist immediately.
#[derive(Clone, Debug)]
pub struct Config {
    store: ConfigStore,
    tlds: Vec<String>,
    favorites: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FavoriteAdd {
    Added(String),
    AlreadyExists(String),
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Stored {
    #[serde(default)]
    tlds: Vec<String>,
    #[serde(default)]
    favorites: Vec<String>,
}

impl Config {
    pub fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self {
            store,
            tlds: sanitize_tlds(stored.tlds).unwrap_or_else(default_tlds),
            favorites: sanitize_favorites(stored.favorites),
        })
    }

    pub fn tlds(&self) -> &[String] {
        &self.tlds
    }

    pub fn favorites(&self) -> &[String] {
        &self.favorites
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.store.path(TOOL)
    }

    pub fn add_favorite(&mut self, favorite: impl AsRef<str>) -> Result<FavoriteAdd> {
        let favorite = canonicalize_query_token(favorite.as_ref())
            .with_context(|| format!("invalid favorite '{}'", favorite.as_ref().trim()))?;

        if self.favorites.iter().any(|existing| existing == &favorite) {
            return Ok(FavoriteAdd::AlreadyExists(favorite));
        }

        self.favorites.push(favorite.clone());
        self.save()?;
        Ok(FavoriteAdd::Added(favorite))
    }

    pub fn set_tlds(&mut self, tlds: Vec<String>) -> Result<()> {
        self.tlds = sanitize_tlds(tlds)
            .ok_or_else(|| anyhow!("TLD set must contain at least one valid suffix"))?;
        self.save()
    }

    fn save(&self) -> Result<()> {
        let stored = Stored {
            tlds: self.tlds.clone(),
            favorites: self.favorites.clone(),
        };
        self.store.save(TOOL, &stored)
    }
}

fn sanitize_tlds(tlds: Vec<String>) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for tld in tlds {
        let Some(cleaned) = canonicalize_suffix(&tld) else {
            continue;
        };
        if !out.iter().any(|existing| existing == &cleaned) {
            out.push(cleaned);
        }
    }

    (!out.is_empty()).then_some(out)
}

fn sanitize_favorites(favorites: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for favorite in favorites {
        let Ok(cleaned) = canonicalize_query_token(&favorite) else {
            continue;
        };
        if !out.iter().any(|existing| existing == &cleaned) {
            out.push(cleaned);
        }
    }

    out
}

fn default_tlds() -> Vec<String> {
    DEFAULT_TLDS.iter().map(|tld| (*tld).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_tlds_and_preserves_order() {
        let input = vec![" .COM ".to_owned(), "ai".to_owned(), ".com".to_owned(), String::new()];
        assert_eq!(sanitize_tlds(input), Some(vec!["com".into(), "ai".into()]));
    }

    #[test]
    fn rejects_tld_sets_without_valid_suffixes() {
        assert_eq!(sanitize_tlds(vec!["bad suffix".to_owned(), String::new()]), None);
    }

    #[test]
    fn sanitizes_favorites_and_preserves_order() {
        let input = vec![
            " ModKit ".to_owned(),
            "modkit".to_owned(),
            "Bücher".to_owned(),
            "bad name".to_owned(),
            "Example.COM.".to_owned(),
        ];
        assert_eq!(
            sanitize_favorites(input),
            vec!["modkit".to_owned(), "xn--bcher-kva".to_owned(), "example.com".to_owned()]
        );
    }

    #[test]
    fn adds_favorites_without_duplicates_and_persists() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kit-domain-config-test-{}", std::process::id()));
        let mut config = Config {
            store: ConfigStore::rooted(dir.clone()),
            tlds: default_tlds(),
            favorites: Vec::new(),
        };

        assert_eq!(config.add_favorite(" ModKit ")?, FavoriteAdd::Added("modkit".to_owned()));
        assert_eq!(config.add_favorite("modkit")?, FavoriteAdd::AlreadyExists("modkit".to_owned()));
        assert_eq!(config.favorites(), ["modkit"]);

        let body = std::fs::read_to_string(config.path())?;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(body.contains("favorites") && body.contains("modkit"));

        Ok(())
    }
}
