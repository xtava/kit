use std::{
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

const ARTIFACT_SCHEMA_VERSION: u32 = 1;
const ARTIFACT_FILE_ATTEMPTS: usize = 32;
const MAX_ARTIFACT_BYTES: u64 = 4 * 1024;
static NEXT_ARTIFACT_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ArtifactIdentity {
    #[serde(rename_all = "camelCase")]
    ContainerImage { source_commit: String, digest: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ContainerImageArtifact {
    schema_version: u32,
    source_commit: String,
    digest: String,
}

pub struct ArtifactCapture {
    path: PathBuf,
}

impl ArtifactCapture {
    pub fn create() -> Result<Self> {
        for _ in 0..ARTIFACT_FILE_ATTEMPTS {
            let nonce = NEXT_ARTIFACT_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kit-deploy-artifact-{}-{nonce}.json", std::process::id()));
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(source)
                        .with_context(|| format!("create artifact result {}", path.display()));
                }
            }
        }
        bail!("could not allocate a deploy artifact result file")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_container_image(&self) -> Result<ArtifactIdentity> {
        let metadata = std::fs::metadata(&self.path)
            .with_context(|| format!("inspect artifact result {}", self.path.display()))?;
        if metadata.len() == 0 {
            bail!("deployment did not write its declared container-image artifact result");
        }
        if metadata.len() > MAX_ARTIFACT_BYTES {
            bail!("deploy artifact result exceeds 4 KiB");
        }
        let bytes = std::fs::read(&self.path)
            .with_context(|| format!("read artifact result {}", self.path.display()))?;
        let result = serde_json::from_slice::<ContainerImageArtifact>(&bytes)
            .context("parse container-image artifact result")?;
        if result.schema_version != ARTIFACT_SCHEMA_VERSION {
            bail!(
                "container-image artifact schema version {} is unsupported; expected {ARTIFACT_SCHEMA_VERSION}",
                result.schema_version
            );
        }
        if !(7..=64).contains(&result.source_commit.len())
            || !result.source_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("container-image artifact has an invalid source commit");
        }
        let Some(digest) = result.digest.strip_prefix("sha256:") else {
            bail!("container-image artifact digest must use sha256");
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("container-image artifact has an invalid sha256 digest");
        }
        Ok(ArtifactIdentity::ContainerImage {
            source_commit: result.source_commit.to_ascii_lowercase(),
            digest: format!("sha256:{}", digest.to_ascii_lowercase()),
        })
    }
}

impl Drop for ArtifactCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
