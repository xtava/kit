use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use ratatui::layout::Rect;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;

const LAYOUT_SCHEMA_VERSION: u32 = 1;
const LAYOUT_FILE: &str = "deploy-layout.json";
const LAYOUT_LOCK_FILE: &str = "deploy-layout.lock";
const RATIO_SCALE: u16 = 1_000;
const DIVIDER_WIDTH: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SplitRatio(u16);

impl SplitRatio {
    pub const fn new_unchecked(value: u16) -> Self {
        Self(value)
    }

    fn from_divider(divider: u16, available: u16) -> Self {
        if available == 0 {
            return Self::new_unchecked(RATIO_SCALE / 2);
        }
        let scaled = (u32::from(divider) * u32::from(RATIO_SCALE) / u32::from(available))
            .clamp(1, u32::from(RATIO_SCALE - 1));
        Self::new_unchecked(scaled as u16)
    }

    fn desired_cells(self, available: u16) -> u16 {
        ((u32::from(available) * u32::from(self.0) + u32::from(RATIO_SCALE / 2))
            / u32::from(RATIO_SCALE)) as u16
    }
}

impl<'de> Deserialize<'de> for SplitRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        if !(1..RATIO_SCALE).contains(&value) {
            return Err(D::Error::custom(format!(
                "split ratio must be between 1 and {}",
                RATIO_SCALE - 1
            )));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitSurface {
    Browse,
    Versions,
    Running,
}

impl SplitSurface {
    pub const fn minimums(self) -> SplitMinimums {
        match self {
            Self::Browse => SplitMinimums { first: 24, second: 32 },
            Self::Versions => SplitMinimums { first: 34, second: 28 },
            Self::Running => SplitMinimums { first: 28, second: 36 },
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
            browse: SplitRatio::new_unchecked(420),
            versions: SplitRatio::new_unchecked(600),
            running: SplitRatio::new_unchecked(430),
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
    pub content: Rect,
    pub first: Rect,
    pub second: Rect,
    pub separator: Rect,
    pub separator_hit_region: Rect,
}

impl LayoutFrame {
    pub fn split(surface: SplitSurface, content: Rect, ratio: SplitRatio) -> Self {
        let available = content.width.saturating_sub(DIVIDER_WIDTH);
        let first_width =
            effective_divider(ratio.desired_cells(available), available, surface.minimums());
        let separator_width = u16::from(content.width > 0);
        let separator_x = content.x.saturating_add(first_width);
        let second_x = separator_x.saturating_add(separator_width);
        let second_width = available.saturating_sub(first_width);
        let hit_start = separator_x.saturating_sub(1).max(content.x);
        let content_end = content.x.saturating_add(content.width);
        let hit_end =
            separator_x.saturating_add(separator_width).saturating_add(1).min(content_end);

        Self {
            surface: Some(surface),
            content,
            first: Rect::new(content.x, content.y, first_width, content.height),
            second: Rect::new(second_x, content.y, second_width, content.height),
            separator: Rect::new(separator_x, content.y, separator_width, content.height),
            separator_hit_region: Rect::new(
                hit_start,
                content.y,
                hit_end.saturating_sub(hit_start),
                content.height,
            ),
        }
    }

    pub fn contains_separator(self, column: u16, row: u16) -> bool {
        contains(self.separator_hit_region, column, row)
    }

    pub fn ratio_for_column(self, column: u16) -> Option<SplitRatio> {
        let surface = self.surface?;
        let available = self.content.width.saturating_sub(DIVIDER_WIDTH);
        if available == 0 {
            return None;
        }
        let requested = column.saturating_sub(self.content.x).min(available);
        let divider = effective_divider(requested, available, surface.minimums());
        Some(SplitRatio::from_divider(divider, available))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitDrag {
    pub surface: SplitSurface,
    pub start_ratio: SplitRatio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitMinimums {
    first: u16,
    second: u16,
}

fn effective_divider(requested: u16, available: u16, minimums: SplitMinimums) -> u16 {
    if available >= minimums.first.saturating_add(minimums.second) {
        requested.clamp(minimums.first, available.saturating_sub(minimums.second))
    } else if available >= 2 {
        requested.clamp(1, available - 1)
    } else {
        requested.min(available)
    }
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[derive(Clone, Debug)]
pub struct LayoutStore {
    dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("resolve Kit state directory")]
    StateDirectory,
    #[error("create deploy layout state directory {}: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("open deploy layout lock {}: {source}", path.display())]
    OpenLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("lock deploy layout {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
    #[error("write deploy layout state {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
        let _lock = self.lock()?;
        let document =
            DeployLayoutDocument { schema_version: LAYOUT_SCHEMA_VERSION, splits: layout };
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        self.write_bytes(&self.path(), &bytes)
    }

    fn lock(&self) -> Result<File, LayoutError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|source| LayoutError::CreateDirectory { path: self.dir.clone(), source })?;
        let path = self.dir.join(LAYOUT_LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| LayoutError::OpenLock { path: path.clone(), source })?;
        file.lock().map_err(|source| LayoutError::Lock { path, source })?;
        Ok(file)
    }

    fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), LayoutError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|source| LayoutError::CreateDirectory { path: self.dir.clone(), source })?;
        let temp = self.dir.join(format!(".deploy-layout-{}.tmp", std::process::id()));
        let result = (|| {
            let mut file = File::create(&temp)
                .map_err(|source| LayoutError::Write { path: temp.clone(), source })?;
            file.write_all(bytes)
                .map_err(|source| LayoutError::Write { path: temp.clone(), source })?;
            file.sync_all().map_err(|source| LayoutError::Write { path: temp.clone(), source })?;
            std::fs::rename(&temp, path)
                .map_err(|source| LayoutError::Write { path: path.to_path_buf(), source })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
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
            browse: SplitRatio::new_unchecked(310),
            versions: SplitRatio::new_unchecked(520),
            running: SplitRatio::new_unchecked(740),
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
        let ratio = SplitRatio::new_unchecked(900);
        let frame = LayoutFrame::split(SplitSurface::Browse, Rect::new(0, 0, 30, 10), ratio);

        assert!(frame.first.width > 0);
        assert!(frame.second.width > 0);
        assert_eq!(frame.first.width + frame.separator.width + frame.second.width, 30);
        assert_eq!(ratio, SplitRatio::new_unchecked(900));
    }

    #[test]
    fn separator_hit_region_tracks_rendered_divider() {
        let frame = LayoutFrame::split(
            SplitSurface::Running,
            Rect::new(10, 5, 101, 20),
            SplitRatio::new_unchecked(430),
        );

        assert!(frame.contains_separator(frame.separator.x, 10));
        assert!(frame.contains_separator(frame.separator.x.saturating_sub(1), 10));
        assert!(!frame.contains_separator(10, 10));
        assert_eq!(frame.ratio_for_column(60), Some(SplitRatio::new_unchecked(500)));
    }
}
