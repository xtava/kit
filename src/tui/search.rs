//! Shared fuzzy indexing and frecency for interactive Kit catalogs.
//!
//! Tools own discovery, filtering, and presentation. This module owns the reusable mechanics:
//! background Nucleo indexing, deterministic fuzzy ranking, and concurrency-safe frecency storage.

use std::{
    collections::HashMap,
    fs::File,
    hash::Hash,
    io::Read,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use nucleo::{
    pattern::{CaseMatching, Normalization},
    Config as NucleoConfig, Nucleo,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

use crate::framework::{AtomicFileError, AtomicFileWriter};

use super::fuzzy;

const FRECENCY_SCHEMA_VERSION: u32 = 1;
const MAX_FRECENCY_ENTRIES: usize = 512;
const MAX_RESCORED_MATCHES: usize = 256;
const MATCH_TICK_MILLIS: u64 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    Text,
    Path,
}

impl SearchMode {
    fn nucleo_config(self) -> NucleoConfig {
        match self {
            Self::Text => NucleoConfig::DEFAULT,
            Self::Path => NucleoConfig::DEFAULT.match_paths(),
        }
    }

    fn matcher(self, query: &str) -> fuzzy::Matcher {
        match self {
            Self::Text => fuzzy::Matcher::case_insensitive(query),
            Self::Path => fuzzy::Matcher::paths(query),
        }
    }
}

struct Indexed<T> {
    item: Arc<T>,
    text: String,
}

pub struct SearchMatch<T> {
    pub item: Arc<T>,
    pub score: u64,
}

/// A reusable one-column background fuzzy index for typed tool-owned items.
pub struct FuzzyIndex<T: Send + Sync + 'static> {
    entries: Vec<Arc<Indexed<T>>>,
    matcher: Nucleo<Arc<Indexed<T>>>,
    mode: SearchMode,
    query: String,
}

impl<T: Send + Sync + 'static> FuzzyIndex<T> {
    pub fn new(mode: SearchMode, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            entries: Vec::new(),
            matcher: Nucleo::new(mode.nucleo_config(), Arc::new(wake), Some(1), 1),
            mode,
            query: String::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn items(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|entry| entry.item.as_ref())
    }

    pub fn replace(&mut self, entries: impl IntoIterator<Item = (T, String)>) {
        self.matcher.restart(true);
        self.entries = entries
            .into_iter()
            .map(|(item, text)| Arc::new(Indexed { item: Arc::new(item), text }))
            .collect();
        let injector = self.matcher.injector();
        for entry in &self.entries {
            injector.push(Arc::clone(entry), |entry, columns| {
                columns[0] = entry.text.as_str().into();
            });
        }
    }

    /// Returns `None` while the background matcher is still processing the current query.
    pub fn search(&mut self, query: &str) -> Option<Vec<SearchMatch<T>>> {
        self.update_query(query);
        if self.matcher.tick(MATCH_TICK_MILLIS).running {
            return None;
        }

        let mut scorer = self.mode.matcher(query);
        let mut matches = self
            .matcher
            .snapshot()
            .matched_items(..)
            .take(MAX_RESCORED_MATCHES)
            .filter_map(|matched| {
                let entry = matched.data;
                let score = scorer.score(&entry.text)?;
                Some((score, entry.text.as_str(), Arc::clone(&entry.item)))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
        Some(matches.into_iter().map(|(score, _, item)| SearchMatch { item, score }).collect())
    }

    fn update_query(&mut self, query: &str) {
        if query == self.query {
            return;
        }
        let append = query.starts_with(&self.query);
        self.matcher.pattern.reparse(0, query, CaseMatching::Ignore, Normalization::Smart, append);
        self.query.clear();
        self.query.push_str(query);
    }
}

#[derive(Clone, Debug)]
pub struct Frecency<K> {
    entries: HashMap<K, FrequencyEntry>,
    pending: HashMap<K, FrequencyEntry>,
}

impl<K> Default for Frecency<K> {
    fn default() -> Self {
        Self { entries: HashMap::new(), pending: HashMap::new() }
    }
}

impl<K> Frecency<K>
where
    K: Clone + Eq + Hash + Ord,
{
    pub fn record(&mut self, key: K) {
        self.record_at(key, unix_time());
    }

    pub fn score(&self, key: &K) -> u64 {
        self.score_at(key, unix_time())
    }

    pub fn describe(&self, key: &K) -> Option<String> {
        self.describe_at(key, unix_time())
    }

    pub fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    fn record_at(&mut self, key: K, now: u64) {
        let entry = self.entries.entry(key.clone()).or_default();
        entry.visits = entry.visits.saturating_add(1);
        entry.last_opened = now;
        let pending = self.pending.entry(key).or_default();
        pending.visits = pending.visits.saturating_add(1);
        pending.last_opened = pending.last_opened.max(now);
        self.prune(now);
    }

    fn score_at(&self, key: &K, now: u64) -> u64 {
        self.entries.get(key).map_or(0, |entry| entry.score(now))
    }

    fn describe_at(&self, key: &K, now: u64) -> Option<String> {
        let entry = self.entries.get(key)?;
        let age = now.saturating_sub(entry.last_opened);
        let ago = match age {
            0..=59 => "now".to_owned(),
            60..=3_599 => format!("{}m ago", age / 60),
            3_600..=86_399 => format!("{}h ago", age / 3_600),
            86_400..=604_799 => format!("{}d ago", age / 86_400),
            _ => format!("{}w ago", age / 604_800),
        };
        Some(format!("{}× · {ago}", entry.visits))
    }

    fn prune(&mut self, now: u64) {
        while self.entries.len() > MAX_FRECENCY_ENTRIES {
            let Some(stale) = self
                .entries
                .iter()
                .min_by(|left, right| {
                    left.1
                        .score(now)
                        .cmp(&right.1.score(now))
                        .then_with(|| left.1.last_opened.cmp(&right.1.last_opened))
                        .then_with(|| left.0.cmp(right.0))
                })
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&stale);
        }
    }

    #[cfg(test)]
    pub(crate) fn visits(&self, key: &K) -> u32 {
        self.entries.get(key).map_or(0, |entry| entry.visits)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrequencyEntry {
    visits: u32,
    last_opened: u64,
}

impl FrequencyEntry {
    fn score(&self, now: u64) -> u64 {
        let age = now.saturating_sub(self.last_opened);
        let recency = match age {
            0..=3_599 => 400,
            3_600..=86_399 => 300,
            86_400..=604_799 => 200,
            604_800..=2_591_999 => 100,
            _ => 0,
        };
        u64::from(self.visits.min(50)) * 25 + recency
    }
}

#[derive(Debug)]
pub struct FrecencyStore {
    dir: PathBuf,
    namespace: String,
}

#[derive(Debug, Error)]
pub enum FrecencyError {
    #[error("invalid frecency namespace {0:?}; use lowercase letters, digits, '-' or '_'")]
    InvalidNamespace(String),
    #[error("resolve Kit state directory")]
    StateDirectory,
    #[error(transparent)]
    Storage(#[from] AtomicFileError),
    #[error("read frecency {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse frecency {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("frecency {} uses schema version {actual}; expected {expected}", path.display())]
    Schema { path: PathBuf, actual: u32, expected: u32 },
    #[error("serialize frecency: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrecencyDocument<K> {
    schema_version: u32,
    entries: Vec<PersistedFrequencyEntry<K>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedFrequencyEntry<K> {
    #[serde(alias = "path")]
    key: K,
    visits: u32,
    last_opened: u64,
}

impl FrecencyStore {
    pub fn bootstrap(namespace: impl Into<String>) -> Result<Self, FrecencyError> {
        let project = ProjectDirs::from("", "", "kit").ok_or(FrecencyError::StateDirectory)?;
        let dir = project.state_dir().unwrap_or_else(|| project.data_local_dir()).to_path_buf();
        Self::new(dir, namespace.into())
    }

    pub fn load<K>(&self) -> Result<Frecency<K>, FrecencyError>
    where
        K: DeserializeOwned + Eq + Hash,
    {
        let path = self.path();
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Frecency::default());
            }
            Err(source) => return Err(FrecencyError::Read { path, source }),
        };
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(|source| FrecencyError::Read { path: path.clone(), source })?;
        let document = serde_json::from_str::<FrecencyDocument<K>>(&raw)
            .map_err(|source| FrecencyError::Parse { path: path.clone(), source })?;
        if document.schema_version != FRECENCY_SCHEMA_VERSION {
            return Err(FrecencyError::Schema {
                path,
                actual: document.schema_version,
                expected: FRECENCY_SCHEMA_VERSION,
            });
        }
        let entries = document
            .entries
            .into_iter()
            .map(|entry| {
                (entry.key, FrequencyEntry { visits: entry.visits, last_opened: entry.last_opened })
            })
            .collect();
        Ok(Frecency { entries, pending: HashMap::new() })
    }

    pub fn save<K>(&self, frecency: &mut Frecency<K>) -> Result<(), FrecencyError>
    where
        K: Clone + DeserializeOwned + Eq + Hash + Ord + Serialize,
    {
        let writer = self.writer();
        let _lock = writer.lock()?;
        let mut merged = self.load()?;
        for (key, pending) in &frecency.pending {
            let entry = merged.entries.entry(key.clone()).or_default();
            entry.visits = entry.visits.saturating_add(pending.visits);
            entry.last_opened = entry.last_opened.max(pending.last_opened);
        }
        merged.prune(unix_time());
        let mut entries = merged
            .entries
            .iter()
            .map(|(key, entry)| PersistedFrequencyEntry {
                key: key.clone(),
                visits: entry.visits,
                last_opened: entry.last_opened,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        let document = FrecencyDocument { schema_version: FRECENCY_SCHEMA_VERSION, entries };
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        writer.replace(&self.path(), &bytes)?;
        *frecency = merged;
        Ok(())
    }

    fn new(dir: PathBuf, namespace: String) -> Result<Self, FrecencyError> {
        if namespace.is_empty()
            || !namespace.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
            })
        {
            return Err(FrecencyError::InvalidNamespace(namespace));
        }
        Ok(Self { dir, namespace })
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("{}-frecency.json", self.namespace))
    }

    fn writer(&self) -> AtomicFileWriter {
        AtomicFileWriter::new(
            &self.dir,
            format!("{}-frecency.lock", self.namespace),
            format!(".{}-frecency", self.namespace),
        )
    }

    #[cfg(test)]
    fn rooted(dir: PathBuf, namespace: &str) -> Self {
        Self::new(dir, namespace.to_owned()).expect("valid test namespace")
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Eq, PartialEq)]
    struct Item {
        id: u8,
    }

    fn temp_dir(name: &str) -> PathBuf {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("kit-search-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn typed_index_is_independent_of_tool_domain() {
        let mut index = FuzzyIndex::new(SearchMode::Text, || {});
        index.replace([
            (Item { id: 1 }, "alpha target".to_owned()),
            (Item { id: 2 }, "beta target".to_owned()),
        ]);

        let matches = index.search("alpha").expect("synchronous small index");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].item.id, 1);
    }

    #[test]
    fn replacing_index_removes_stale_items() {
        let mut index = FuzzyIndex::new(SearchMode::Path, || {});
        index.replace([(Item { id: 1 }, "old/file.md".to_owned())]);
        index.replace([(Item { id: 2 }, "new/file.md".to_owned())]);

        assert_eq!(index.len(), 1);
        assert!(index.search("old").expect("synchronous small index").is_empty());
        assert_eq!(index.search("new").expect("synchronous small index")[0].item.id, 2);
    }

    #[test]
    fn frecency_balances_frequency_and_recency_for_generic_keys() {
        let mut frecency = Frecency::default();
        frecency.record_at("recent".to_owned(), 1_000_000);
        for _ in 0..10 {
            frecency.record_at("frequent".to_owned(), 1_000_000 - 8 * 86_400);
        }

        assert!(
            frecency.score_at(&"recent".to_owned(), 1_000_000)
                > frecency.score_at(&"frequent".to_owned(), 1_000_000)
        );
        frecency.record_at("frequent".to_owned(), 1_000_000);
        assert!(
            frecency.score_at(&"frequent".to_owned(), 1_000_000)
                > frecency.score_at(&"recent".to_owned(), 1_000_000)
        );
    }

    #[test]
    fn namespaced_store_round_trip_preserves_typed_keys() -> Result<(), FrecencyError> {
        let dir = temp_dir("round-trip");
        let store = FrecencyStore::rooted(dir.clone(), "targets");
        let mut frecency = Frecency::default();
        frecency.record_at(42_u64, 123);
        store.save(&mut frecency)?;

        let loaded = store.load::<u64>()?;
        assert_eq!(loaded.visits(&42), 1);
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }

    #[test]
    fn stale_sessions_merge_pending_visits_instead_of_overwriting() -> Result<(), FrecencyError> {
        let dir = temp_dir("merge");
        let store = FrecencyStore::rooted(dir.clone(), "commands");
        let mut first = store.load::<String>()?;
        let mut second = store.load::<String>()?;
        first.record_at("deploy".to_owned(), 100);
        second.record_at("deploy".to_owned(), 200);

        store.save(&mut first)?;
        store.save(&mut second)?;

        assert_eq!(store.load::<String>()?.visits(&"deploy".to_owned()), 2);
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }

    #[test]
    fn rejects_namespace_path_traversal() {
        assert!(matches!(
            FrecencyStore::new(PathBuf::new(), "../render".to_owned()),
            Err(FrecencyError::InvalidNamespace(_))
        ));
    }
}
