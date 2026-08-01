use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use crate::onepassword::OpClient;
use crate::onepassword::OpEnvironment;

use super::{
    cloudflare::CloudflarePagesClient,
    config::{DeployTarget, LoadedPlan},
    journal::JournalStore,
    runner::{RunOperation, RunSpec, RunTargetSpec},
    source,
    state::RunIntent,
};

pub async fn prepare_run(
    loaded: &LoadedPlan,
    intent: RunIntent,
    review_targets: Vec<DeployTarget>,
    journal_store: &JournalStore,
) -> Result<RunSpec> {
    let op = OpClient::new();
    match intent {
        RunIntent::DeployProduction => {
            prepare_production_with_op(loaded, review_targets, journal_store, &op).await
        }
        RunIntent::DeployPreview { branch, .. } => {
            let target = review_targets
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("selected preview Target no longer exists"))?;
            let working_dir = target_working_dir(&loaded.base_dir, &target);
            let version = journal_store.current_version(&target.id, &working_dir).await?;
            let source_roots = target_source_roots(&loaded.base_dir, &target);
            let source = source::inspect(&working_dir, &source_roots).await?;
            let environment = target_environment(loaded, &target.id)?;
            let production = cloudflare_production_branch(&target, &environment, &op)
                .await?
                .ok_or_else(|| anyhow!("selected Target has no Cloudflare Pages backend"))?;
            ensure_preview_branch(&branch, &production)?;
            Ok(RunSpec {
                base_dir: loaded.base_dir.clone(),
                operation: RunOperation::DeployPreview,
                targets: vec![RunTargetSpec {
                    target,
                    version,
                    source,
                    branch: Some(branch),
                    environment,
                }],
            })
        }
        RunIntent::Rollback { version, .. } => {
            let target = review_targets
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("selected Target has no rollback Steps"))?;
            let environment = target_environment(loaded, &target.id)?;
            Ok(RunSpec {
                base_dir: loaded.base_dir.clone(),
                operation: RunOperation::Rollback { selected_version: version.clone() },
                targets: vec![RunTargetSpec {
                    target,
                    version,
                    source: None,
                    branch: None,
                    environment,
                }],
            })
        }
        RunIntent::CloudflarePagesRollback { .. } => {
            Err(anyhow!("Cloudflare Pages rollback must use the platform API"))
        }
    }
}

pub async fn prepare_production(
    loaded: &LoadedPlan,
    targets: Vec<DeployTarget>,
    journal_store: &JournalStore,
) -> Result<RunSpec> {
    prepare_production_with_op(loaded, targets, journal_store, &OpClient::new()).await
}

async fn prepare_production_with_op(
    loaded: &LoadedPlan,
    targets: Vec<DeployTarget>,
    journal_store: &JournalStore,
    op: &OpClient,
) -> Result<RunSpec> {
    let mut prepared = Vec::with_capacity(targets.len());
    for target in targets {
        let working_dir = target_working_dir(&loaded.base_dir, &target);
        let version = journal_store.current_version(&target.id, &working_dir).await?;
        let source_roots = target_source_roots(&loaded.base_dir, &target);
        let source = source::inspect(&working_dir, &source_roots).await?;
        let environment = target_environment(loaded, &target.id)?;
        let branch = cloudflare_production_branch(&target, &environment, op).await?;
        prepared.push(RunTargetSpec { target, version, source, branch, environment });
    }
    Ok(RunSpec {
        base_dir: loaded.base_dir.clone(),
        operation: RunOperation::DeployProduction,
        targets: prepared,
    })
}

async fn cloudflare_production_branch(
    target: &DeployTarget,
    environment: &OpEnvironment,
    op: &OpClient,
) -> Result<Option<String>> {
    let Some(client) = CloudflarePagesClient::for_target(target, environment, op).await? else {
        return Ok(None);
    };
    Ok(Some(client.get_project().await?.production_branch))
}

pub fn ensure_preview_branch(branch: &str, production: &str) -> Result<()> {
    if branch == production {
        bail!("'{branch}' is Cloudflare's production branch; use a production deploy instead")
    }
    Ok(())
}

pub fn target_environment(loaded: &LoadedPlan, target_id: &str) -> Result<OpEnvironment> {
    loaded
        .environments
        .get(target_id)
        .cloned()
        .ok_or_else(|| anyhow!("Target '{target_id}' has no loaded environment"))
}

pub fn target_working_dir(base_dir: &Path, target: &DeployTarget) -> PathBuf {
    resolve_target_path(base_dir, target.working_dir.as_deref())
}

fn target_source_roots(base_dir: &Path, target: &DeployTarget) -> Vec<PathBuf> {
    target
        .source_roots
        .iter()
        .map(|path| resolve_target_path(base_dir, Some(path)))
        .collect()
}

fn resolve_target_path(base_dir: &Path, path: Option<&Path>) -> PathBuf {
    match path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => base_dir.join(path),
        None => base_dir.to_path_buf(),
    }
}
