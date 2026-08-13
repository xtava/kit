use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::Path,
};

use anyhow::{Context as _, Result};

use super::{
    graph::{self, ProjectedRelation, ProjectionLine},
    protocol::{
        CheckEntryConfigFreshness, CheckIncompleteReason, CheckResult, CheckVerdict, CompilerExit,
        CompilerOutputEvidence, DiagnoseIncompleteReason, DiagnoseProject, DiagnoseResult,
        DiagnoseVerdict, Diagnostic, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity,
        LocusAcquisitionState, LocusBlock, LocusDiscoveryState, LocusEvidenceCapture,
        LocusFreshness, LocusGapFamily, LocusObligationState, LocusOmission, LocusRequirementState,
        LocusResult, LocusSeedResult, LocusSessionIntegrity, RequestedDocumentFreshness,
        ServiceInfo, TraceAdviceReason, TraceBoundary, TraceDirection, TraceEdge, TraceGap,
        TraceIdentityGapReason, TraceLocation, TracePackageScope, TraceProjectContext, TraceResult,
    },
};

const TEXT_EVIDENCE_LIMIT: usize = 256;

pub fn diagnose_text(service: &ServiceInfo, result: &DiagnoseResult) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Document Diagnostics\n");
    detail(&mut output, "Authority", "warm language service");
    detail(
        &mut output,
        "Verdict",
        match result.verdict {
            DiagnoseVerdict::NoLocalDiagnostics => "no-local-diagnostics",
            DiagnoseVerdict::LocalDiagnostics => "local-diagnostics",
            DiagnoseVerdict::Incomplete { .. } => "incomplete",
        },
    );
    detail(&mut output, "Workspace", &display_workspace(&service.workspace));
    detail(&mut output, "Documents", &result.documents.len().to_string());
    detail(&mut output, "Project contexts", "selected project only");
    detail(&mut output, "Dependency freshness", "unchecked");
    detail(
        &mut output,
        "Diagnostics",
        &format!(
            "{} errors · {} warnings · {} total",
            result.summary.errors, result.summary.warnings, result.summary.total
        ),
    );
    detail(&mut output, "Elapsed", &format!("{} ms", result.timing.elapsed_ms));
    if !result.documents.is_empty() {
        let _ = writeln!(output, "\nDocuments");
        for document in &result.documents {
            let project = match &document.selected_project {
                DiagnoseProject::Configured { config } => {
                    format!("selected project {}", escaped_path(config))
                }
                DiagnoseProject::Inferred => "selected inferred project".to_owned(),
            };
            let _ = writeln!(output, "- {} — {project}", escaped_path(&document.file));
        }
    }
    if !result.diagnostics.is_empty() {
        let _ = writeln!(output, "\nDiagnostics");
        for diagnostic in &result.diagnostics {
            let _ = writeln!(output, "{}", diagnostic_text(diagnostic));
        }
    }
    if result.summary.omitted > 0 {
        let _ = writeln!(
            output,
            "\n- {} diagnostics omitted by the output bound",
            result.summary.omitted
        );
    }
    if let DiagnoseVerdict::Incomplete { reasons } = &result.verdict {
        let _ = writeln!(output, "\nIncomplete");
        for reason in reasons {
            let _ = writeln!(output, "- {}", diagnose_incomplete_reason(reason));
        }
    }
    if let RequestedDocumentFreshness::Changed { files } = &result.requested_document_freshness {
        let _ = writeln!(output, "\nChanged requested documents");
        for file in files {
            let _ = writeln!(output, "- {}", escaped_path(&file.file));
        }
    }
    let _ = writeln!(
        output,
        "\nScope: explicit documents only; workspace/project completeness was not checked."
    );
    output.trim_end().to_owned()
}

pub fn check_text(result: &CheckResult) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "TypeScript Project Check\n");
    detail(&mut output, "Authority", "native compiler project");
    detail(
        &mut output,
        "Verdict",
        match result.verdict {
            CheckVerdict::CompilerReportedNoDiagnostics => "compiler-reported-no-diagnostics",
            CheckVerdict::DiagnosticsPresent => "diagnostics-present",
            CheckVerdict::Incomplete { .. } => "incomplete",
        },
    );
    detail(&mut output, "Workspace", &display_workspace(&result.workspace));
    detail(&mut output, "Project", &escaped_path(&result.project.config));
    detail(
        &mut output,
        "Coverage",
        &format!(
            "{} root files · {} project references",
            result.coverage.root_files, result.coverage.project_references
        ),
    );
    detail(
        &mut output,
        "Compiler",
        &format!(
            "{} ({})",
            escaped_path(&result.invocation.launcher),
            result.invocation.server_version
        ),
    );
    detail(&mut output, "Exit", &compiler_exit_text(result.exit));
    detail(&mut output, "Input freshness", "unchecked");
    detail(
        &mut output,
        "Diagnostics",
        &format!(
            "{} errors · {} warnings · {} total",
            result.summary.errors, result.summary.warnings, result.summary.total
        ),
    );
    detail(&mut output, "Elapsed", &format!("{} ms", result.timing.elapsed_ms));
    if !result.diagnostics.is_empty() {
        let _ = writeln!(output, "\nDiagnostics");
        for diagnostic in &result.diagnostics {
            let _ = writeln!(output, "{}", diagnostic_text(diagnostic));
        }
    }
    if result.summary.omitted > 0 {
        let _ = writeln!(
            output,
            "\n- {} diagnostics omitted by the output bound",
            result.summary.omitted
        );
    }
    if let CheckVerdict::Incomplete { reasons } = &result.verdict {
        let _ = writeln!(output, "\nIncomplete");
        for reason in reasons {
            let _ = writeln!(output, "- {}", check_incomplete_reason(reason));
        }
    }
    match &result.entry_config_freshness {
        CheckEntryConfigFreshness::Verified => {}
        CheckEntryConfigFreshness::Changed { .. } => {
            let _ = writeln!(output, "\nEntry config changed during the check.");
        }
        CheckEntryConfigFreshness::Unreadable { detail } => {
            let _ = writeln!(output, "\nEntry config could not be re-read: {detail}");
        }
    }
    match &result.output {
        CompilerOutputEvidence::Classified => {}
        CompilerOutputEvidence::Unclassified { stdout, stderr }
        | CompilerOutputEvidence::Truncated { stdout, stderr, .. } => {
            if !stdout.is_empty() {
                let _ = writeln!(output, "\nCompiler stdout\n{stdout}");
            }
            if !stderr.is_empty() {
                let _ = writeln!(output, "\nCompiler stderr\n{stderr}");
            }
        }
    }
    output.trim_end().to_owned()
}

pub fn diagnostic_failure_text(title: &str, failure: &str) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "{title}\n");
    detail(&mut output, "Verdict", "incomplete");
    let _ = writeln!(output, "\nOperational failure\n- {failure}");
    output.trim_end().to_owned()
}

fn diagnose_incomplete_reason(reason: &DiagnoseIncompleteReason) -> String {
    match reason {
        DiagnoseIncompleteReason::ChangedInput => {
            "a requested document changed during diagnosis".to_owned()
        }
        DiagnoseIncompleteReason::DiagnosticLimit { observed, retained } => {
            format!("diagnostic limit retained {retained} of {observed}")
        }
        DiagnoseIncompleteReason::DiagnosticDetailLimit { omitted } => {
            format!("{omitted} diagnostic details were omitted")
        }
        DiagnoseIncompleteReason::UnspecifiedSeverity { diagnostics } => {
            format!("{diagnostics} diagnostics had no severity")
        }
        DiagnoseIncompleteReason::UnknownSeverity { diagnostics } => {
            format!("{diagnostics} diagnostics used an unknown severity")
        }
    }
}

fn check_incomplete_reason(reason: &CheckIncompleteReason) -> String {
    match reason {
        CheckIncompleteReason::EntryConfigChanged => {
            "entry config changed during the check".to_owned()
        }
        CheckIncompleteReason::EntryConfigUnreadable => {
            "entry config could not be re-read".to_owned()
        }
        CheckIncompleteReason::OutputTruncated => "compiler output was truncated".to_owned(),
        CheckIncompleteReason::UnclassifiedOutput => {
            "compiler output contained unclassified records".to_owned()
        }
        CheckIncompleteReason::DiagnosticLimit => "diagnostic limit was reached".to_owned(),
        CheckIncompleteReason::DiagnosticDetailLimit => {
            "diagnostic detail limit was reached".to_owned()
        }
        CheckIncompleteReason::NoRootFiles => "the project contains no root files".to_owned(),
        CheckIncompleteReason::ProjectReferencesNotChecked { references } => {
            format!("{references} project references were not checked")
        }
        CheckIncompleteReason::ProjectDiagnostic { diagnostics } => {
            format!("{diagnostics} project-level diagnostics prevent a coverage claim")
        }
        CheckIncompleteReason::InconsistentCompilerResult => {
            "compiler exit and diagnostic output disagree".to_owned()
        }
        CheckIncompleteReason::UnexpectedExit => "compiler returned an unexpected exit".to_owned(),
        CheckIncompleteReason::DeadlineExceeded => "compiler deadline was exceeded".to_owned(),
        CheckIncompleteReason::Cancelled => "compiler run was cancelled".to_owned(),
        CheckIncompleteReason::ExternalTermination => {
            "compiler was terminated externally".to_owned()
        }
    }
}

fn compiler_exit_text(exit: CompilerExit) -> String {
    match exit {
        CompilerExit::Code { code } => code.to_string(),
        CompilerExit::Signal { signal } => format!("signal {signal}"),
        CompilerExit::NotObserved => "not observed".to_owned(),
    }
}

fn diagnostic_text(diagnostic: &Diagnostic) -> String {
    let location = match &diagnostic.location {
        DiagnosticLocation::SourceRange { file, range } => {
            format!("{}:{}:{}", escaped_path(file), range.start.line, range.start.character)
        }
        DiagnosticLocation::SourcePoint { file, position } => {
            format!("{}:{}:{}", escaped_path(file), position.line, position.character)
        }
        DiagnosticLocation::Project { config } => escaped_path(config),
    };
    let severity = match diagnostic.severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Information => "information",
        DiagnosticSeverity::Hint => "hint",
        DiagnosticSeverity::Unspecified => "diagnostic",
        DiagnosticSeverity::Unknown { .. } => "unknown",
    };
    let code = match &diagnostic.code {
        DiagnosticCode::Absent => String::new(),
        DiagnosticCode::Number { value } => format!(" TS{value}"),
        DiagnosticCode::Text { value } => format!(" {value}"),
    };
    format!("{location}  {severity}{code}: {}", one_line(&diagnostic.message))
}

pub fn trace_text(service: &ServiceInfo, result: &TraceResult) -> Result<String> {
    let title = result
        .target
        .as_ref()
        .and_then(|id| result.nodes.get(id))
        .map(|node| node.name.as_str())
        .unwrap_or(result.selector.as_str());
    let workspace_name = display_workspace(&service.workspace);
    let status = result.status.label();
    let resolved = result
        .target
        .as_ref()
        .and_then(|id| result.nodes.get(id))
        .map(|node| location_text(&service.workspace, &node.definition))
        .unwrap_or_else(|| "—".to_owned());

    let mut output = String::new();
    let _ = writeln!(output, "Call Trace › {title}\n");
    let _ = writeln!(output, "Trace Details");
    detail(&mut output, "Workspace", &workspace_name);
    detail(&mut output, "Direction", result.direction.label());
    detail(&mut output, "Status", status);
    detail(&mut output, "Resolved Target", &resolved);
    detail(&mut output, "Elapsed", &format!("{} ms", result.timing.elapsed_ms));
    detail(&mut output, "Native Requests", &result.timing.native_requests.to_string());
    detail(&mut output, "Instance", &service.instance_id);
    detail(&mut output, "Child", &service.child.run_id);
    detail(&mut output, "Request", &service.request_count.to_string());

    let _ = writeln!(output, "\nSummary");
    let _ = writeln!(
        output,
        "{} Observed Leaves  ·  {} Nodes  ·  {} Edges",
        result.summary.observed_leaves, result.summary.nodes, result.summary.edges
    );
    if result.summary.cycle_components > 0
        || result.summary.boundaries > 0
        || result.summary.truncated
    {
        let _ = writeln!(
            output,
            "{} Cyclic Components  ·  {} Boundaries  ·  {}",
            result.summary.cycle_components,
            result.summary.boundaries,
            if result.summary.truncated { "Truncated" } else { "Complete" }
        );
    }

    if !result.candidates.is_empty() && result.target.is_none() {
        let _ = writeln!(output, "\nCandidates");
        for candidate in &result.candidates {
            let name = candidate
                .detail
                .as_ref()
                .map(|detail| format!("{}  {detail}", candidate.name))
                .unwrap_or_else(|| candidate.name.clone());
            let _ = writeln!(
                output,
                "{:<44} {}",
                name,
                location_text(&service.workspace, &candidate.location)
            );
        }
    }

    if let Some(target) = result.target.as_deref() {
        let projection = graph::project(target, result.direction, &result.nodes, &result.edges)
            .map_err(anyhow::Error::msg)
            .context("project merged tsgo call tree")?;
        let observed_leaves =
            result.observed_leaves.iter().map(String::as_str).collect::<BTreeSet<_>>();
        let mut boundaries = BTreeMap::<&str, Vec<&TraceBoundary>>::new();
        for boundary in &result.boundaries {
            boundaries.entry(boundary.node.as_str()).or_default().push(boundary);
        }

        let _ = writeln!(output, "\nCall Tree");
        for line in &projection.lines {
            let node = &result.nodes[&line.node];
            let edge = line.edge_index.map(|index| &result.edges[index]);
            let location =
                edge.and_then(|edge| edge.call_sites.first()).unwrap_or(&node.definition);
            let tree = tree_prefix(line);
            let relation = match line.relation {
                ProjectedRelation::Target | ProjectedRelation::Expanded => {
                    format!("[{}]", line.reference)
                }
                ProjectedRelation::CycleReference => format!("⇄ [{}]", line.reference),
                ProjectedRelation::SharedReference => format!("↩ [{}]", line.reference),
            };
            let label = format!("{tree}{relation} {}()", node.name);
            let annotation =
                if matches!(line.relation, ProjectedRelation::Target | ProjectedRelation::Expanded)
                {
                    trace_node_annotation(
                        result.direction,
                        observed_leaves.contains(line.node.as_str()),
                        boundaries.get(line.node.as_str()).map(Vec::as_slice).unwrap_or(&[]),
                        edge,
                        node.generated_aliases.len(),
                    )
                } else {
                    callsite_annotation(edge)
                };
            let _ = writeln!(
                output,
                "{label:<72} {}{annotation}",
                location_text(&service.workspace, location)
            );
        }

        if !result.cycle_components.is_empty() {
            let _ = writeln!(output, "\nCyclic Components");
            for component in &result.cycle_components {
                let members = component
                    .iter()
                    .map(|node_id| {
                        let reference = projection.references[node_id];
                        format!("[{reference}] {}()", result.nodes[node_id].name)
                    })
                    .collect::<Vec<_>>()
                    .join("  ↔  ");
                let _ = writeln!(output, "- {members}");
            }
        }
    }

    let _ = writeln!(output, "\nCapture Coverage");
    detail(&mut output, "Workspace", "project files not enumerated by the native server");
    detail(
        &mut output,
        "Documents",
        &format!(
            "{} captured · {} project contexts omitted",
            result.coverage.documents.len(),
            result.coverage.omitted_project_contexts
        ),
    );
    for document in result.coverage.documents.iter().take(TEXT_EVIDENCE_LIMIT) {
        let project = match &document.project {
            TraceProjectContext::Configured { config } => {
                format!("configured by {}", escaped_path(config))
            }
            TraceProjectContext::Inferred => "inferred project".to_owned(),
            TraceProjectContext::NotQueried { .. } => "project context not queried".to_owned(),
            TraceProjectContext::Unavailable { detail } => {
                format!("project context unavailable: {}", one_line(detail))
            }
        };
        let _ = writeln!(
            output,
            "- {} · {} · {project}",
            escaped_path(&document.file),
            document.sync.label()
        );
    }
    if result.coverage.documents.len() > TEXT_EVIDENCE_LIMIT {
        let _ = writeln!(
            output,
            "- {} captured documents omitted from text output",
            result.coverage.documents.len() - TEXT_EVIDENCE_LIMIT
        );
    }

    if !result.scope.source_roots.is_empty()
        || !matches!(&result.scope.package, TracePackageScope::Disabled)
    {
        let _ = writeln!(output, "\nExpansion Scope");
        for root in &result.scope.source_roots {
            let _ = writeln!(output, "- within {}", escaped_path(root));
        }
        match &result.scope.package {
            TracePackageScope::Disabled => {}
            TracePackageScope::Unresolved => {
                let _ = writeln!(output, "- package boundary requested but no package.json found");
            }
            TracePackageScope::Enabled { root } => {
                let _ = writeln!(output, "- stop at package {}", escaped_path(root));
            }
        }
    }

    let aliases =
        result.nodes.values().filter(|node| !node.generated_aliases.is_empty()).collect::<Vec<_>>();
    if !aliases.is_empty() {
        let _ = writeln!(output, "\nUnified Generated Identities");
        for node in aliases {
            let canonical = location_text_full(&service.workspace, &node.definition);
            let generated = node
                .generated_aliases
                .iter()
                .map(|alias| location_text_full(&service.workspace, alias))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(output, "- {}() · {canonical} ← {generated}", node.name);
        }
    }

    if !result.gaps.is_empty() {
        let _ = writeln!(output, "\nEvidence Gaps");
        let caller_gaps = result
            .gaps
            .iter()
            .filter(|gap| matches!(gap, TraceGap::CallerAbsenceUnproven { .. }))
            .count();
        if caller_gaps > 0 {
            let _ = writeln!(
                output,
                "- {caller_gaps} observed leaves returned no callers; absence is not proven"
            );
        }
        for gap in &result.gaps {
            let TraceGap::GeneratedIdentityUnresolved { node, declaration, reason } = gap else {
                continue;
            };
            let name = result.nodes.get(node).map_or(node.as_str(), |node| node.name.as_str());
            let _ = writeln!(
                output,
                "- {}() at {} · {}",
                name,
                location_text_full(&service.workspace, declaration),
                trace_identity_gap_text(reason)
            );
        }
    }

    if !result.advice.is_empty() {
        let _ = writeln!(output, "\nAdvice");
        for advice in &result.advice {
            let reason = match advice.reason {
                TraceAdviceReason::BroadExpansion => "broad expansion",
            };
            let _ = writeln!(
                output,
                "- direct ownership is already visible; retry with --max-depth {} ({reason})",
                advice.suggested_max_depth
            );
        }
    }

    if !result.truncation_reasons.is_empty() {
        let _ = writeln!(output, "\nLimits");
        for reason in &result.truncation_reasons {
            let _ = writeln!(output, "- {reason}");
        }
    }

    Ok(output.trim_end().to_owned())
}

pub fn locus_text(service: &ServiceInfo, result: &LocusResult) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Placement Evidence › {}\n", one_line(&result.goal));
    let _ = writeln!(output, "Case Details");
    detail(&mut output, "Workspace", &display_workspace(&service.workspace));
    detail(&mut output, "Status", result.status.label());
    detail(&mut output, "Freshness", freshness_label(&result.freshness));
    detail(&mut output, "Fingerprint", &result.fingerprint);
    detail(&mut output, "Elapsed", &format!("{} ms", result.timing.elapsed_ms));
    detail(&mut output, "Native Requests", &result.timing.native_requests.to_string());
    detail(&mut output, "Instance", &service.instance_id);

    let complete = result
        .acquisitions
        .iter()
        .filter(|item| matches!(&item.state, LocusAcquisitionState::CompleteWithinCapture { .. }))
        .count();
    let no_call_item = result
        .acquisitions
        .iter()
        .filter(|item| matches!(&item.state, LocusAcquisitionState::NoCallItem))
        .count();
    let cut = result
        .acquisitions
        .iter()
        .filter(|item| matches!(&item.state, LocusAcquisitionState::Cut { .. }))
        .count();
    let unsupported = result
        .acquisitions
        .iter()
        .filter(|item| matches!(&item.state, LocusAcquisitionState::Unsupported { .. }))
        .count();
    let failed = result
        .acquisitions
        .iter()
        .filter(|item| matches!(&item.state, LocusAcquisitionState::Failed { .. }))
        .count();
    detail(
        &mut output,
        "Capture",
        &format!(
            "{complete} complete-within-capture · {no_call_item} no-call-item · {cut} cut · {unsupported} unsupported · {failed} failed"
        ),
    );

    let _ = writeln!(output, "\nSeeds");
    for seed in &result.seeds {
        match seed {
            LocusSeedResult::Resolved { seed_id, label, anchor, .. } => {
                let _ = writeln!(
                    output,
                    "[{seed_id}] {}  {}",
                    one_line(label),
                    location_text_full(&service.workspace, &anchor.location)
                );
            }
            LocusSeedResult::Ambiguous { seed_id, label, candidates, observed, .. } => {
                let _ = writeln!(
                    output,
                    "[{seed_id}] {}  ambiguous: {} of {observed} candidates retained",
                    one_line(label),
                    candidates.len()
                );
            }
            LocusSeedResult::NotFound { seed_id, label, .. } => {
                let _ = writeln!(output, "[{seed_id}] {}  not found", one_line(label));
            }
            LocusSeedResult::Failed { seed_id, label, reason, session_integrity, .. } => {
                let _ = writeln!(
                    output,
                    "[{seed_id}] {}  failed ({}) · {}",
                    one_line(label),
                    integrity_label(*session_integrity),
                    one_line(reason)
                );
            }
            LocusSeedResult::AmbiguousCallItem {
                seed_id,
                label,
                anchor,
                acquisition_id,
                candidates,
                observed,
                ..
            } => {
                let _ = writeln!(
                    output,
                    "[{seed_id}] {}  {} · ambiguous call item in {acquisition_id}: {} of {observed} candidates retained",
                    one_line(label),
                    location_text_full(&service.workspace, &anchor.location),
                    candidates.len()
                );
            }
        }
    }

    let _ = writeln!(output, "\nAcquisitions");
    for acquisition in &result.acquisitions {
        let _ = writeln!(
            output,
            "[{}] {} · seed {} · {} · {} evidence",
            acquisition.id,
            acquisition.operation.relation().label(),
            acquisition.seed_id,
            acquisition_state_text(&acquisition.state),
            acquisition.evidence_ids.len()
        );
        if let Some(prepare) = &acquisition.prepare {
            let _ = writeln!(
                output,
                "  prepared root {}",
                location_text_full(&service.workspace, &prepare.semantic_root.location)
            );
        }
    }

    if !result.evidence.is_empty() {
        let _ = writeln!(output, "\nEvidence");
        for evidence in result.evidence.iter().take(TEXT_EVIDENCE_LIMIT) {
            let sites = if evidence.call_sites.is_empty() {
                String::new()
            } else {
                format!(" · {} callsites", evidence.call_sites.len())
            };
            let _ = writeln!(
                output,
                "[{}] {} · seed {} · {} → {} · {}{}",
                evidence.id,
                evidence.relation.label(),
                evidence.seed_id,
                location_text_full(&service.workspace, &evidence.source.location),
                location_text_full(&service.workspace, &evidence.target.location),
                match evidence.capture {
                    LocusEvidenceCapture::CompleteWithinCapture => "complete-within-capture",
                    LocusEvidenceCapture::RetainedBeforeCut => "retained-before-cut",
                },
                sites
            );
        }
        if result.evidence.len() > TEXT_EVIDENCE_LIMIT {
            let _ = writeln!(
                output,
                "- text projection omitted {} evidence relations; JSON retains them",
                result.evidence.len() - TEXT_EVIDENCE_LIMIT
            );
        }
    }

    let _ = writeln!(output, "\nInspect Anchors");
    if result.candidates.is_empty() {
        let _ = writeln!(output, "- none within the declared discovery rules");
    }
    for candidate in &result.candidates {
        let _ = writeln!(
            output,
            "[{}] {}  {} · discovered by {}",
            candidate.id,
            one_line(&candidate.label),
            location_text_full(&service.workspace, &candidate.anchor.location),
            candidate.discovered_by
        );
        for obligation in &candidate.obligations {
            let _ = writeln!(output, "  {}", one_line(&obligation.obligation_id));
            for requirement in &obligation.requirements {
                let _ = writeln!(
                    output,
                    "    {}: {}",
                    requirement.acquisition_id,
                    requirement_state_text(&requirement.state)
                );
            }
        }
    }

    let _ = writeln!(output, "\nObligations");
    for obligation in &result.obligations {
        let state = match obligation.state {
            LocusObligationState::ClosedWithinDeclaredCapture => "closed within declared capture",
            LocusObligationState::Open => "open",
        };
        let _ =
            writeln!(output, "[{}] {} · {state}", obligation.id, one_line(&obligation.statement));
    }

    let _ = writeln!(output, "\nDiscovery Receipts");
    for receipt in &result.discovery_receipts {
        let state = match &receipt.state {
            LocusDiscoveryState::Applied => "applied".to_owned(),
            LocusDiscoveryState::NoMatch => "no-match".to_owned(),
            LocusDiscoveryState::IncompleteEvidence => "incomplete-evidence".to_owned(),
            LocusDiscoveryState::Cut { omission } => {
                format!("cut: {} omitted", omission_text(omission))
            }
        };
        let candidates = if receipt.candidate_ids.is_empty() {
            "none".to_owned()
        } else {
            receipt.candidate_ids.join(", ")
        };
        let _ = writeln!(output, "[{}] {state} → {candidates}", receipt.rule_id);
    }

    if !result.blocks.is_empty() {
        let _ = writeln!(output, "\nBlocks");
        for block in &result.blocks {
            let _ = writeln!(output, "- {}", block_text(block));
        }
    }
    if !result.gaps.is_empty() {
        let _ = writeln!(output, "\nDeclared Gaps");
        for gap in &result.gaps {
            let requirement = if gap.required { "required" } else { "optional" };
            let _ = writeln!(
                output,
                "[{}] {} · {requirement} · {}",
                gap.id,
                gap_family_label(gap.family),
                one_line(&gap.statement)
            );
        }
    }
    if !result.assumptions.is_empty() {
        let _ = writeln!(output, "\nCase Assumptions");
        for assumption in &result.assumptions {
            let _ = writeln!(output, "[{}] {}", assumption.id, one_line(&assumption.statement));
        }
    }
    if !result.non_goals.is_empty() {
        let _ = writeln!(output, "\nNon-goals");
        for non_goal in &result.non_goals {
            let _ = writeln!(output, "[{}] {}", non_goal.id, one_line(&non_goal.statement));
        }
    }

    output.trim_end().to_owned()
}

fn freshness_label(freshness: &LocusFreshness) -> &'static str {
    match freshness {
        LocusFreshness::Checked { .. } => "checked",
        LocusFreshness::ChangedObservedInput { .. } => "changed-observed-input",
    }
}

fn acquisition_state_text(state: &LocusAcquisitionState) -> String {
    match state {
        LocusAcquisitionState::CompleteWithinCapture { retained } => {
            format!("complete-within-capture: {retained} retained")
        }
        LocusAcquisitionState::Cut { retained, cuts } => {
            let reasons = cuts
                .iter()
                .map(|cut| format!("{:?}: {}", cut.reason, omission_text(&cut.omission)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("cut: {retained} retained · {reasons}")
        }
        LocusAcquisitionState::Unsupported { reason } => {
            format!("unsupported: {}", one_line(reason))
        }
        LocusAcquisitionState::NoCallItem => "no-call-item".to_owned(),
        LocusAcquisitionState::AmbiguousCallItem { candidates, observed } => {
            format!("ambiguous-call-item: {} of {observed} candidates retained", candidates.len())
        }
        LocusAcquisitionState::Failed { reason, session_integrity } => {
            format!("failed ({}): {}", integrity_label(*session_integrity), one_line(reason))
        }
    }
}

fn requirement_state_text(state: &LocusRequirementState) -> String {
    match state {
        LocusRequirementState::Witnessed { evidence_ids, observed } => {
            format!("witnessed ({observed} observed; evidence {})", evidence_ids.join(","))
        }
        LocusRequirementState::WitnessedBeforeCut { evidence_ids, observed } => format!(
            "witnessed-before-cut ({observed} observed; evidence {})",
            evidence_ids.join(",")
        ),
        LocusRequirementState::NotObservedWithinCompleteAcquisition => {
            "not-observed-within-complete-acquisition".to_owned()
        }
        LocusRequirementState::OpenCut => "open-cut".to_owned(),
        LocusRequirementState::OpenUnsupported => "open-unsupported".to_owned(),
        LocusRequirementState::OpenFailed => "open-failed".to_owned(),
        LocusRequirementState::AcceptedNoCallItem => "accepted-no-call-item".to_owned(),
        LocusRequirementState::OpenNoCallItem => "open-no-call-item".to_owned(),
        LocusRequirementState::OpenDeclaredGap { gap_id } => {
            format!("open-declared-gap ({gap_id})")
        }
    }
}

fn omission_text(omission: &LocusOmission) -> String {
    match omission {
        LocusOmission::Known { count } => count.to_string(),
        LocusOmission::Unknown => "unknown".to_owned(),
    }
}

fn integrity_label(integrity: LocusSessionIntegrity) -> &'static str {
    match integrity {
        LocusSessionIntegrity::Preserved => "session preserved",
        LocusSessionIntegrity::Lost => "session lost",
    }
}

fn block_text(block: &LocusBlock) -> String {
    match block {
        LocusBlock::AmbiguousSeed { seed_id } => format!("seed {seed_id} is ambiguous"),
        LocusBlock::SeedNotFound { seed_id } => format!("seed {seed_id} was not found"),
        LocusBlock::SeedFailure { seed_id, session_integrity } => {
            format!("seed {seed_id} failed ({})", integrity_label(*session_integrity))
        }
        LocusBlock::ChangedObservedInput => {
            "an observed source file changed before return".to_owned()
        }
        LocusBlock::LostSessionDuringAcquisition { acquisition_id } => {
            format!("session integrity was lost during acquisition {acquisition_id}")
        }
        LocusBlock::AmbiguousCallItem { acquisition_id } => {
            format!("acquisition {acquisition_id} returned ambiguous call items")
        }
    }
}

fn gap_family_label(family: LocusGapFamily) -> &'static str {
    match family {
        LocusGapFamily::EventReducer => "event-reducer",
        LocusGapFamily::DependencyInjectionRegistration => "dependency-injection-registration",
        LocusGapFamily::RuntimeObservation => "runtime-observation",
        LocusGapFamily::ResourceFlow => "resource-flow",
        LocusGapFamily::UpstreamPolicy => "upstream-policy",
        LocusGapFamily::OtherLanguage => "other-language",
        LocusGapFamily::DynamicDispatch => "dynamic-dispatch",
        LocusGapFamily::GeneratedCode => "generated-code",
    }
}

fn one_line(value: &str) -> String {
    let mut output = String::new();
    let mut previous_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            if !previous_space && !output.is_empty() {
                output.push(' ');
            }
            previous_space = true;
            continue;
        }
        previous_space = false;
        if character.is_control() || unsafe_terminal_format(character) {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output.trim_end().to_owned()
}

fn unsafe_terminal_format(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

fn location_text_full(workspace: &Path, location: &TraceLocation) -> String {
    let file = location.file.strip_prefix(workspace).unwrap_or(&location.file);
    format!("{}:{}:{}", escaped_path(file), location.line, location.character)
}

fn escaped_path(path: &Path) -> String {
    one_line(&path.to_string_lossy())
}

fn detail(output: &mut String, label: &str, value: &str) {
    let _ = writeln!(output, "{label:<17}{value}");
}

fn tree_prefix(line: &ProjectionLine) -> String {
    if line.relation == ProjectedRelation::Target {
        return String::new();
    }
    let mut prefix = String::new();
    for continuation in &line.ancestor_continuations {
        prefix.push_str(if *continuation { "│  " } else { "   " });
    }
    prefix.push_str(if line.last_sibling { "└─ " } else { "├─ " });
    prefix
}

fn trace_node_annotation(
    direction: TraceDirection,
    observed_leaf: bool,
    boundaries: &[&TraceBoundary],
    edge: Option<&TraceEdge>,
    aliases: usize,
) -> String {
    let mut annotations = Vec::new();
    if observed_leaf {
        annotations.push(
            match direction {
                TraceDirection::Callers => "no-returned-callers",
                TraceDirection::Callees => "no-returned-callees",
            }
            .to_owned(),
        );
    }
    for boundary in boundaries {
        let mut label = boundary.kind.label().to_owned();
        if boundary.omitted_relations > 0 {
            label.push_str(&format!(": {} omitted", boundary.omitted_relations));
        }
        annotations.push(label);
    }
    if let Some(sites) = edge.map(|edge| edge.call_sites.len()).filter(|sites| *sites > 1) {
        annotations.push(format!("{sites} callsites"));
    }
    if aliases > 0 {
        annotations.push(format!("{aliases} generated aliases"));
    }
    if annotations.is_empty() {
        String::new()
    } else {
        format!("  {}", annotations.join(" · "))
    }
}

fn callsite_annotation(edge: Option<&TraceEdge>) -> String {
    edge.map(|edge| edge.call_sites.len())
        .filter(|sites| *sites > 1)
        .map(|sites| format!("  {sites} callsites"))
        .unwrap_or_default()
}

fn location_text(workspace: &Path, location: &TraceLocation) -> String {
    let file = location.file.strip_prefix(workspace).unwrap_or(&location.file);
    format!("{}:{}", escaped_path(file), location.line)
}

fn display_workspace(workspace: &Path) -> String {
    let raw = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let mut characters = raw.chars();
    match characters.next() {
        Some(first) => one_line(&first.to_uppercase().chain(characters).collect::<String>()),
        None => "Workspace".to_owned(),
    }
}

fn trace_identity_gap_text(reason: &TraceIdentityGapReason) -> String {
    match reason {
        TraceIdentityGapReason::SourceDefinitionUnsupported => {
            "native source-definition support was not advertised".to_owned()
        }
        TraceIdentityGapReason::NoSourceDefinition => {
            "generated declaration had no source definition".to_owned()
        }
        TraceIdentityGapReason::AmbiguousSourceDefinition { observed } => {
            format!("generated declaration had {observed} source definitions")
        }
        TraceIdentityGapReason::SourceOutsideWorkspace => {
            "mapped source is outside the workspace".to_owned()
        }
        TraceIdentityGapReason::SourceMapMissing => {
            "generated JavaScript has no source map".to_owned()
        }
        TraceIdentityGapReason::SourceMapInvalid { detail } => {
            format!("source map is unusable: {}", one_line(detail))
        }
        TraceIdentityGapReason::SourcePositionUnmapped => {
            "generated position has no source-map segment".to_owned()
        }
        TraceIdentityGapReason::SourcePreparationNotUnique { observed } => {
            format!("canonical source prepared {observed} matching call items")
        }
        TraceIdentityGapReason::NativeRequestFailed { detail } => {
            format!("native normalization request failed: {}", one_line(detail))
        }
    }
}

#[cfg(test)]
fn node_annotation(
    observed_leaf: bool,
    boundaries: &[&TraceBoundary],
    edge: Option<&TraceEdge>,
) -> String {
    trace_node_annotation(TraceDirection::Callees, observed_leaf, boundaries, edge, 0)
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{TraceBoundary, TraceBoundaryKind};
    use super::node_annotation;

    #[test]
    fn typed_cuts_are_visible_and_not_reported_as_endpoints() {
        let boundary = TraceBoundary {
            node: "target".to_owned(),
            kind: TraceBoundaryKind::MaxDepth,
            omitted_relations: 3,
        };

        assert_eq!(node_annotation(false, &[&boundary], None), "  max-depth: 3 omitted");
    }
}
