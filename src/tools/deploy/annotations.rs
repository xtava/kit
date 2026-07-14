use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::framework::{AtomicFileError, AtomicFileWriter};

const ANNOTATIONS_SCHEMA_VERSION: u32 = 1;
const ANNOTATIONS_FILE: &str = "deploy-annotations.json";
const LOCK_FILE: &str = "deploy-annotations.lock";

/// An operator's local note on one platform deployment, kept outside the platform
/// because Cloudflare Pages has no annotation surface of its own.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    #[serde(default)]
    pub error: bool,
    #[serde(default)]
    pub note: Option<String>,
}

impl Annotation {
    pub fn is_empty(&self) -> bool {
        !self.error && self.note.as_deref().is_none_or(str::is_empty)
    }
}

/// Every operator annotation, keyed by the platform's deployment id.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployAnnotations {
    pub schema_version: u32,
    #[serde(default)]
    pub deployments: BTreeMap<String, Annotation>,
}

impl Default for DeployAnnotations {
    fn default() -> Self {
        Self { schema_version: ANNOTATIONS_SCHEMA_VERSION, deployments: BTreeMap::new() }
    }
}

impl DeployAnnotations {
    pub fn get(&self, deployment_id: &str) -> Option<&Annotation> {
        self.deployments.get(deployment_id)
    }

    /// Flip the error flag for one deployment, returning the new flag value.
    pub fn toggle_error(&mut self, deployment_id: &str) -> bool {
        let entry = self.deployments.entry(deployment_id.to_owned()).or_default();
        entry.error = !entry.error;
        let error = entry.error;
        self.prune(deployment_id);
        error
    }

    pub fn set_note(&mut self, deployment_id: &str, note: Option<String>) {
        let note = note.map(|note| note.trim().to_owned()).filter(|note| !note.is_empty());
        self.deployments.entry(deployment_id.to_owned()).or_default().note = note;
        self.prune(deployment_id);
    }

    fn prune(&mut self, deployment_id: &str) {
        if self.deployments.get(deployment_id).is_some_and(Annotation::is_empty) {
            self.deployments.remove(deployment_id);
        }
    }
}

#[derive(Clone, Debug)]
pub struct AnnotationStore {
    dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum AnnotationError {
    #[error("resolve Kit state directory")]
    StateDirectory,
    #[error(transparent)]
    Storage(#[from] AtomicFileError),
    #[error("read deploy annotations {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse deploy annotations {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("deploy annotations {} uses schema version {actual}; expected {expected}", path.display())]
    Schema { path: PathBuf, actual: u32, expected: u32 },
    #[error("serialize deploy annotations: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl AnnotationStore {
    pub fn bootstrap() -> Result<Self, AnnotationError> {
        let project = ProjectDirs::from("", "", "kit").ok_or(AnnotationError::StateDirectory)?;
        let dir = project.state_dir().unwrap_or_else(|| project.data_local_dir()).to_path_buf();
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub fn rooted(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(ANNOTATIONS_FILE)
    }

    pub fn load(&self) -> Result<DeployAnnotations, AnnotationError> {
        load_path(&self.path())
    }

    pub fn save(&self, annotations: &DeployAnnotations) -> Result<(), AnnotationError> {
        let writer = self.writer();
        let _lock = writer.lock()?;
        let mut bytes = serde_json::to_vec_pretty(annotations)?;
        bytes.push(b'\n');
        writer.replace(&self.path(), &bytes)?;
        Ok(())
    }

    fn writer(&self) -> AtomicFileWriter {
        AtomicFileWriter::new(&self.dir, LOCK_FILE, ".deploy-annotations")
    }
}

fn load_path(path: &Path) -> Result<DeployAnnotations, AnnotationError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeployAnnotations::default());
        }
        Err(source) => return Err(AnnotationError::Read { path: path.to_path_buf(), source }),
    };
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|source| AnnotationError::Read { path: path.to_path_buf(), source })?;
    let annotations = serde_json::from_str::<DeployAnnotations>(&raw)
        .map_err(|source| AnnotationError::Parse { path: path.to_path_buf(), source })?;
    if annotations.schema_version != ANNOTATIONS_SCHEMA_VERSION {
        return Err(AnnotationError::Schema {
            path: path.to_path_buf(),
            actual: annotations.schema_version,
            expected: ANNOTATIONS_SCHEMA_VERSION,
        });
    }
    Ok(annotations)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    fn store() -> AnnotationStore {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        AnnotationStore::rooted(
            std::env::temp_dir()
                .join(format!("kit-deploy-annotations-{}-{id}", std::process::id())),
        )
    }

    #[test]
    fn toggle_and_note_round_trip_through_disk() -> Result<(), AnnotationError> {
        let store = store();
        let mut annotations = store.load()?;
        assert!(annotations.toggle_error("deploy-a"));
        annotations.set_note("deploy-a", Some("  broke sign-in  ".to_owned()));
        store.save(&annotations)?;

        let loaded = store.load()?;
        let _ = std::fs::remove_dir_all(&store.dir);
        let entry = loaded.get("deploy-a").expect("annotation persisted");
        assert!(entry.error);
        assert_eq!(entry.note.as_deref(), Some("broke sign-in"));
        Ok(())
    }

    #[test]
    fn clearing_every_field_prunes_the_entry() {
        let mut annotations = DeployAnnotations::default();
        assert!(annotations.toggle_error("deploy-b"));
        assert!(!annotations.toggle_error("deploy-b"));
        assert!(annotations.get("deploy-b").is_none());

        annotations.set_note("deploy-c", Some("   ".to_owned()));
        assert!(annotations.get("deploy-c").is_none());
    }
}
