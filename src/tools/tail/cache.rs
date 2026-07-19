use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;
const TEXT_LIMIT_BYTES: u64 = 1024 * 1024;
const MANIFEST: &str = "manifest.json";
const PAYLOAD: &str = "payload";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ItemKind {
    Text,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveConflictResolution {
    Rename,
    Replace,
}

#[derive(Clone, Debug)]
pub struct CachedItem {
    pub id: Uuid,
    pub name: String,
    pub received_at: i64,
    pub bytes: u64,
    pub kind: ItemKind,
    directory: PathBuf,
}

impl CachedItem {
    pub fn expires_at(&self) -> i64 {
        self.received_at + RETENTION_SECONDS
    }

    pub fn payload(&self) -> PathBuf {
        self.directory.join(PAYLOAD)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    name: String,
    received_at: i64,
}

#[derive(Clone, Debug)]
pub struct ReceiveCache {
    root: PathBuf,
}

pub struct StagingDirectory {
    root: PathBuf,
    path: PathBuf,
    consumed: bool,
}

impl StagingDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.consumed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl ReceiveCache {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "kit").context("resolve Kit cache directory")?;
        Self::at(dirs.cache_dir().join("tail/received"))
    }

    pub fn at(root: PathBuf) -> Result<Self> {
        create_private_directory(&root)?;
        let root = root.canonicalize().with_context(|| format!("resolve {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn staging_directory(&self) -> Result<StagingDirectory> {
        let path = self.root.join(format!(".incoming-{}", Uuid::new_v4()));
        create_private_directory(&path)?;
        Ok(StagingDirectory { root: self.root.clone(), path, consumed: false })
    }

    pub fn import_staging(&self, mut staging: StagingDirectory) -> Result<Vec<CachedItem>> {
        if staging.root != self.root {
            bail!("receive staging directory belongs to a different cache");
        }
        ensure_direct_child(&self.root, &staging.path, ".incoming-")?;
        let mut imported = Vec::new();
        for entry in fs::read_dir(&staging.path)
            .with_context(|| format!("read {}", staging.path.display()))?
        {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_file() {
                continue;
            }
            match self.import_file(&entry.path()) {
                Ok(item) => imported.push(item),
                Err(error) => {
                    staging.consumed = true;
                    return Err(error.context(format!(
                        "received files preserved in {}",
                        staging.path.display()
                    )));
                }
            }
        }
        fs::remove_dir_all(&staging.path)
            .with_context(|| format!("remove {}", staging.path.display()))?;
        staging.consumed = true;
        Ok(imported)
    }

    pub fn list(&self) -> Result<Vec<CachedItem>> {
        let mut items = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            if let Ok(item) = load_item(id, entry.path()) {
                items.push(item);
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.received_at));
        Ok(items)
    }

    pub fn prune(&self) -> Result<usize> {
        self.prune_at(now_timestamp()?)
    }

    fn prune_at(&self, now: i64) -> Result<usize> {
        let expired =
            self.list()?.into_iter().filter(|item| item.expires_at() <= now).collect::<Vec<_>>();
        for item in &expired {
            self.delete(item)?;
        }
        Ok(expired.len())
    }

    pub fn read_text(&self, item: &CachedItem) -> Result<String> {
        validate_item(&self.root, item)?;
        if item.kind != ItemKind::Text {
            bail!("{} is not UTF-8 text", item.name);
        }
        fs::read_to_string(item.payload()).with_context(|| format!("read {}", item.name))
    }

    pub fn destination_path(&self, item: &CachedItem, destination_directory: &Path) -> PathBuf {
        destination_directory.join(safe_name(&item.name))
    }

    pub fn save_to(
        &self,
        item: &CachedItem,
        destination_directory: &Path,
        conflict: SaveConflictResolution,
    ) -> Result<PathBuf> {
        validate_item(&self.root, item)?;
        fs::create_dir_all(destination_directory)
            .with_context(|| format!("create {}", destination_directory.display()))?;
        let preferred = self.destination_path(item, destination_directory);
        let destination = match conflict {
            SaveConflictResolution::Rename => unique_destination(destination_directory, &item.name),
            SaveConflictResolution::Replace => preferred,
        };
        match conflict {
            SaveConflictResolution::Rename => move_file(&item.payload(), &destination),
            SaveConflictResolution::Replace => replace_file(&item.payload(), &destination),
        }
        .with_context(|| format!("save to {}", destination.display()))?;
        fs::remove_dir_all(&item.directory)
            .with_context(|| format!("remove cache item {}", item.id))?;
        Ok(destination)
    }

    pub fn delete(&self, item: &CachedItem) -> Result<()> {
        validate_item(&self.root, item)?;
        fs::remove_dir_all(&item.directory).with_context(|| format!("delete {}", item.name))
    }

    fn import_file(&self, source: &Path) -> Result<CachedItem> {
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.file_type().is_file() {
            bail!("refusing non-file Taildrop payload {}", source.display());
        }
        let id = Uuid::new_v4();
        let directory = self.root.join(id.to_string());
        create_private_directory(&directory)?;
        let payload = directory.join(PAYLOAD);
        let result = (|| {
            let manifest = Manifest {
                name: source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("taildrop")
                    .to_owned(),
                received_at: now_timestamp()?,
            };
            fs::write(directory.join(MANIFEST), serde_json::to_vec(&manifest)?)?;
            set_private_file(&directory.join(MANIFEST))?;
            fs::rename(source, &payload).context("move Taildrop payload into cache")?;
            set_private_file(&payload)?;
            load_item(id, directory.clone())
        })();
        if result.is_err() {
            if payload.exists() {
                let _ = fs::rename(&payload, source);
            }
            let _ = fs::remove_dir_all(&directory);
        }
        result
    }
}

fn load_item(id: Uuid, directory: PathBuf) -> Result<CachedItem> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(directory.join(MANIFEST))?)?;
    let payload = directory.join(PAYLOAD);
    let metadata = fs::symlink_metadata(&payload)?;
    if !metadata.file_type().is_file() {
        bail!("cache payload is not a regular file");
    }
    let bytes = metadata.len();
    let kind = if bytes <= TEXT_LIMIT_BYTES
        && fs::read(&payload).is_ok_and(|bytes| std::str::from_utf8(&bytes).is_ok())
    {
        ItemKind::Text
    } else {
        ItemKind::File
    };
    Ok(CachedItem {
        id,
        name: manifest.name,
        received_at: manifest.received_at,
        bytes,
        kind,
        directory,
    })
}

fn validate_item(root: &Path, item: &CachedItem) -> Result<()> {
    ensure_direct_child(root, &item.directory, "")?;
    if item.directory.file_name().and_then(|name| name.to_str())
        != Some(item.id.to_string().as_str())
    {
        bail!("cache item identity mismatch");
    }
    let metadata = fs::symlink_metadata(item.payload())?;
    if !metadata.file_type().is_file() {
        bail!("cache payload is not a regular file");
    }
    Ok(())
}

fn ensure_direct_child(root: &Path, path: &Path, prefix: &str) -> Result<()> {
    if path.parent() != Some(root)
        || !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(prefix))
    {
        bail!("refusing path outside the Tail receive cache");
    }
    Ok(())
}

fn unique_destination(directory: &Path, name: &str) -> PathBuf {
    let safe_name = safe_name(name);
    let initial = directory.join(safe_name);
    if !initial.exists() {
        return initial;
    }
    let path = Path::new(safe_name);
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("taildrop");
    let extension = path.extension().and_then(|extension| extension.to_str());
    for suffix in 1.. {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{stem} ({suffix}).{extension}")),
            None => directory.join(format!("{stem} ({suffix})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn safe_name(name: &str) -> &str {
    Path::new(name).file_name().and_then(|name| name.to_str()).unwrap_or("taildrop")
}

fn move_file(source: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            fs::copy(source, destination)?;
            if let Err(error) = fs::remove_file(source) {
                let _ = fs::remove_file(destination);
                return Err(error.into());
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    if destination.is_dir() {
        bail!("refusing to replace destination directory {}", destination.display());
    }
    if !destination.exists() {
        return move_file(source, destination);
    }
    let parent = destination.parent().context("replacement destination has no parent")?;
    let transaction = Uuid::new_v4();
    let temporary = parent.join(format!(".kit-tail-{transaction}.tmp"));
    let backup = parent.join(format!(".kit-tail-{transaction}.bak"));
    fs::copy(source, &temporary)?;
    if let Err(error) = fs::rename(destination, &backup) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::rename(&backup, destination);
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        let _ = fs::rename(&backup, destination);
        return Err(error.into());
    }
    let _ = fs::remove_file(&backup);
    Ok(())
}

fn now_timestamp() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing non-directory or symlink cache path {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_text_and_removes_staging() {
        let root = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        let staging_path = staging.path().to_owned();
        fs::write(staging.path().join("note.txt"), "hello").unwrap();
        let items = cache.import_staging(staging).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Text);
        assert_eq!(cache.read_text(&items[0]).unwrap(), "hello");
        assert!(!staging_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(
                fs::metadata(items[0].payload()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropping_staging_cleans_an_interrupted_receive() {
        let root = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        let path = staging.path().to_owned();
        drop(staging);
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_removes_items_older_than_thirty_days() {
        let root = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        fs::write(staging.path().join("old.txt"), "old").unwrap();
        let item = cache.import_staging(staging).unwrap().remove(0);
        let manifest = Manifest { name: item.name.clone(), received_at: 0 };
        fs::write(item.directory.join(MANIFEST), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(cache.prune().unwrap(), 1);
        assert!(cache.list().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eviction_boundary_keeps_day_29_and_removes_day_30() {
        let root = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let now = 4_000_000;
        for (name, received_at) in
            [("day-29.txt", now - RETENTION_SECONDS + 1), ("day-30.txt", now - RETENTION_SECONDS)]
        {
            let staging = cache.staging_directory().unwrap();
            fs::write(staging.path().join(name), name).unwrap();
            let item = cache.import_staging(staging).unwrap().remove(0);
            let manifest = Manifest { name: item.name, received_at };
            fs::write(item.directory.join(MANIFEST), serde_json::to_vec(&manifest).unwrap())
                .unwrap();
        }
        assert_eq!(cache.prune_at(now).unwrap(), 1);
        assert_eq!(cache.list().unwrap()[0].name, "day-29.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_refuses_an_item_outside_the_cache() {
        let root = test_directory();
        let outside = test_directory();
        let payload = outside.join(PAYLOAD);
        fs::write(&payload, "keep me").unwrap();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let item = CachedItem {
            id: Uuid::new_v4(),
            name: "outside".into(),
            received_at: 0,
            bytes: 7,
            kind: ItemKind::Text,
            directory: outside.clone(),
        };
        assert!(cache.delete(&item).is_err());
        assert_eq!(fs::read_to_string(payload).unwrap(), "keep me");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn save_conflict_can_rename_without_touching_existing_file() {
        let root = test_directory();
        let destination = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        fs::write(staging.path().join("note.txt"), "new").unwrap();
        let item = cache.import_staging(staging).unwrap().remove(0);
        fs::write(destination.join("note.txt"), "old").unwrap();
        let saved = cache.save_to(&item, &destination, SaveConflictResolution::Rename).unwrap();
        assert_eq!(saved.file_name().unwrap(), "note (1).txt");
        assert_eq!(fs::read_to_string(destination.join("note.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(saved).unwrap(), "new");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn save_conflict_can_replace_after_staging_the_new_copy() {
        let root = test_directory();
        let destination = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        fs::write(staging.path().join("note.txt"), "new").unwrap();
        let item = cache.import_staging(staging).unwrap().remove(0);
        let destination_file = destination.join("note.txt");
        fs::write(&destination_file, "old").unwrap();
        let saved = cache.save_to(&item, &destination, SaveConflictResolution::Replace).unwrap();
        assert_eq!(saved, destination_file);
        assert_eq!(fs::read_to_string(saved).unwrap(), "new");
        assert!(cache.list().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinks_received_into_staging() {
        use std::os::unix::fs::symlink;

        let root = test_directory();
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        let outside = root.parent().unwrap().join(format!("outside-{}", Uuid::new_v4()));
        fs::write(&outside, "do not import").unwrap();
        symlink(&outside, staging.path().join("link")).unwrap();
        let items = cache.import_staging(staging).unwrap();
        assert!(items.is_empty());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "do not import");
        fs::remove_file(outside).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_cache_root() {
        use std::os::unix::fs::symlink;

        let parent = test_directory();
        let target = parent.join("target");
        fs::create_dir(&target).unwrap();
        let link = parent.join("link");
        symlink(&target, &link).unwrap();
        assert!(ReceiveCache::at(link).is_err());
        fs::remove_dir_all(parent).unwrap();
    }

    fn test_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kit-tail-cache-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }
}
