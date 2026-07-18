use std::{fs::File, io::Read, path::PathBuf};

use directories::ProjectDirs;
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::framework::{AtomicFileError, AtomicFileWriter};
pub use crate::tui::SplitRatio;
use crate::tui::{SplitFrame, SplitMinimums};

const LAYOUT_SCHEMA_VERSION: u32 = 1;
const LAYOUT_FILE: &str = "deploy-layout.json";
const LAYOUT_LOCK_FILE: &str = "deploy-layout.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitSurface {
    Browse,
    Versions,
    Running,
}

impl SplitSurface {
    pub const fn minimums(self) -> SplitMinimums {
        match self {
            Self::Browse => SplitMinimums::new(24, 32),
            Self::Versions => SplitMinimums::new(34, 28),
            Self::Running => SplitMinimums::new(28, 36),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployLayout {
    pub browse: SplitRatio,
    pub versions: SplitRatio,
    pub running: SplitRatio,
}

impl Default for DeployLayout {
    fn default() -> Self {
        Self {
            browse: SplitRatio::new(420),
            versions: SplitRatio::new(600),
            running: SplitRatio::new(430),
        }
    }
}

impl DeployLayout {
    pub fn ratio(self, surface: SplitSurface) -> SplitRatio {
        match surface {
            SplitSurface::Browse => self.browse,
            SplitSurface::Versions => self.versions,
            SplitSurface::Running => self.running,
        }
    }

    pub fn set_ratio(&mut self, surface: SplitSurface, ratio: SplitRatio) {
        match surface {
            SplitSurface::Browse => self.browse = ratio,
            SplitSurface::Versions => self.versions = ratio,
            SplitSurface::Running => self.running = ratio,
        }
    }

    pub fn reset(&mut self, surface: SplitSurface) -> bool {
        let default = Self::default().ratio(surface);
        let changed = self.ratio(surface) != default;
        self.set_ratio(surface, default);
        changed
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LayoutFrame {
    pub surface: Option<SplitSurface>,
    pub split: SplitFrame,
}

impl LayoutFrame {
    pub fn split(surface: SplitSurface, content: Rect, ratio: SplitRatio) -> Self {
        Self {
            surface: Some(surface),
            split: SplitFrame::horizontal(content, ratio, surface.minimums()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LayoutStore {
    dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("resolve Kit state directory")]
    StateDirectory,
    #[error(transparent)]
    Storage(#[from] AtomicFileError),
    #[error("read deploy layout {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse deploy layout {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("deploy layout {} uses schema version {actual}; expected {expected}", path.display())]
    Schema { path: PathBuf, actual: u32, expected: u32 },
    #[error("serialize deploy layout: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeployLayoutDocument {
    schema_version: u32,
    splits: DeployLayout,
}

impl LayoutStore {
    pub fn bootstrap() -> Result<Self, LayoutError> {
        let project = ProjectDirs::from("", "", "kit").ok_or(LayoutError::StateDirectory)?;
        let dir = project.state_dir().unwrap_or_else(|| project.data_local_dir()).to_path_buf();
        Ok(Self { dir })
    }

    #[cfg(test)]
    fn rooted(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(LAYOUT_FILE)
    }

    pub fn load(&self) -> Result<DeployLayout, LayoutError> {
        let path = self.path();
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DeployLayout::default());
            }
            Err(source) => return Err(LayoutError::Read { path, source }),
        };
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(|source| LayoutError::Read { path: path.clone(), source })?;
        let document = serde_json::from_str::<DeployLayoutDocument>(&raw)
            .map_err(|source| LayoutError::Parse { path: path.clone(), source })?;
        if document.schema_version != LAYOUT_SCHEMA_VERSION {
            return Err(LayoutError::Schema {
                path,
                actual: document.schema_version,
                expected: LAYOUT_SCHEMA_VERSION,
            });
        }
        Ok(document.splits)
    }

    pub fn save(&self, layout: DeployLayout) -> Result<(), LayoutError> {
        let writer = self.writer();
        let _lock = writer.lock()?;
        let document =
            DeployLayoutDocument { schema_version: LAYOUT_SCHEMA_VERSION, splits: layout };
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        writer.replace(&self.path(), &bytes)?;
        Ok(())
    }

    fn writer(&self) -> AtomicFileWriter {
        AtomicFileWriter::new(&self.dir, LAYOUT_LOCK_FILE, ".deploy-layout")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    fn store() -> LayoutStore {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        LayoutStore::rooted(
            std::env::temp_dir().join(format!("kit-deploy-layout-{}-{id}", std::process::id())),
        )
    }

    #[test]
    fn layout_round_trip_preserves_typed_ratios() -> Result<(), LayoutError> {
        let store = store();
        let layout = DeployLayout {
            browse: SplitRatio::new(310),
            versions: SplitRatio::new(520),
            running: SplitRatio::new(740),
        };

        store.save(layout)?;
        assert_eq!(store.load()?, layout);
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn invalid_ratio_is_an_actionable_parse_error() -> Result<(), Box<dyn std::error::Error>> {
        let store = store();
        std::fs::create_dir_all(&store.dir)?;
        std::fs::write(
            store.path(),
            r#"{"schema_version":1,"splits":{"browse":0,"versions":600,"running":430}}"#,
        )?;

        let error = match store.load() {
            Ok(_) => return Err("zero ratio was accepted".into()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("split ratio must be between 1 and 999"));
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn newer_schema_is_rejected_without_rewriting_state() -> Result<(), Box<dyn std::error::Error>>
    {
        let store = store();
        std::fs::create_dir_all(&store.dir)?;
        let raw = r#"{"schema_version":2,"splits":{"browse":420,"versions":600,"running":430}}"#;
        std::fs::write(store.path(), raw)?;

        let error = match store.load() {
            Ok(_) => return Err("newer schema was accepted".into()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("schema version 2; expected 1"));
        assert_eq!(std::fs::read_to_string(store.path())?, raw);
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn narrow_terminal_clamps_without_changing_preference() {
        let ratio = SplitRatio::new(900);
        let frame = LayoutFrame::split(SplitSurface::Browse, Rect::new(0, 0, 30, 10), ratio);

        assert_eq!(frame.surface, Some(SplitSurface::Browse));
        assert!(frame.split.first.width > 0);
        assert!(frame.split.second.width > 0);
        assert_eq!(
            frame.split.first.width + frame.split.separator.width + frame.split.second.width,
            30
        );
        assert_eq!(ratio, SplitRatio::new(900));
    }
}
