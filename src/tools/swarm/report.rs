use std::path::PathBuf;

use thiserror::Error;

use crate::framework::{AtomicFileError, AtomicFileWriter};

use super::{model::SwarmProjection, store::SwarmStore};

#[derive(Debug, Error)]
pub enum ReportError {
    #[error("cannot render report for non-terminal swarm {0}")]
    NonTerminal(super::model::SwarmId),
    #[error(transparent)]
    Atomic(#[from] AtomicFileError),
    #[error("set report permissions {}: {source}", path.display())]
    Permissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn render(projection: &SwarmProjection) -> Result<String, ReportError> {
    if !projection.status.is_terminal() {
        return Err(ReportError::NonTerminal(projection.spec.id.clone()));
    }
    let mut report = format!(
        "# Swarm {}\n\n- Status: `{:?}`\n- Terminal sequence: `{}`\n\n",
        projection.spec.id, projection.status, projection.last_sequence
    );
    if let Some(result) = projection.result.as_ref() {
        report.push_str("## Answer\n\n");
        report.push_str(result.answer.trim());
        report.push_str("\n\n## Consensus\n\n");
        push_list(&mut report, &result.consensus);
        report.push_str("\n## Dissent\n\n");
        push_list(&mut report, &result.dissent);
        report.push_str("\n## Confidence\n\n");
        report.push_str(result.confidence.trim());
        report.push('\n');
    } else if let Some(failure) = projection.failure.as_ref() {
        report.push_str("## Failure\n\n");
        report.push_str(failure.trim());
        report.push('\n');
    }
    Ok(report)
}

pub fn write(store: &SwarmStore, projection: &SwarmProjection) -> Result<PathBuf, ReportError> {
    let report = render(projection)?;
    let directory = store.run_dir(&projection.spec.id);
    let path = directory.join("report.md");
    let writer = AtomicFileWriter::new(&directory, ".report.lock", ".report");
    let _lock = writer.lock()?;
    writer.replace(&path, report.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|source| ReportError::Permissions { path: path.clone(), source })?;
    }
    Ok(path)
}

fn push_list(report: &mut String, items: &[String]) {
    if items.is_empty() {
        report.push_str("- None\n");
        return;
    }
    for item in items {
        report.push_str("- ");
        report.push_str(item.trim());
        report.push('\n');
    }
}
