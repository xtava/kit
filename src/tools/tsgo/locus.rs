use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};

use super::protocol::{
    LocusAcquisition, LocusAcquisitionResult, LocusAcquisitionState, LocusBlock, LocusCandidate,
    LocusCandidateObligation, LocusCapture, LocusDeclaredGap, LocusDiscoveryReceipt,
    LocusDiscoveryRule, LocusDiscoveryState, LocusDiscoveryStrategy, LocusEvidence,
    LocusEvidenceCapture, LocusFreshness, LocusGap, LocusGapProvenance, LocusObligation,
    LocusObligationResult, LocusObligationState, LocusOperation, LocusRecheckValue, LocusRequest,
    LocusRequirementResult, LocusRequirementState, LocusResult, LocusSeedResult,
    LocusSessionIntegrity, LocusStatus, TraceDirection, TraceLocation, TraceSelector,
    LOCUS_SCHEMA_VERSION, MAX_LOCUS_CANDIDATES, MAX_LOCUS_CASE_ITEMS, MAX_LOCUS_ID_BYTES,
    MAX_LOCUS_LABEL_BYTES, MAX_LOCUS_LOCATIONS, MAX_LOCUS_MATRIX_CELLS, MAX_LOCUS_OBSERVED_FILES,
    MAX_LOCUS_REQUIREMENTS, MAX_LOCUS_TEXT_BYTES, MAX_LOCUS_TOTAL_AMBIGUITY_CANDIDATES,
    MAX_LOCUS_TOTAL_CALL_SITES, MAX_LOCUS_TOTAL_EVIDENCE, MAX_LOCUS_WITNESSES_PER_REQUIREMENT,
    MAX_TRACE_DEPTH, MAX_TRACE_NODES,
};

pub fn validate_request(request: &LocusRequest) -> Result<()> {
    if request.schema_version != LOCUS_SCHEMA_VERSION {
        bail!(
            "locus case schema {} is unsupported; expected {}",
            request.schema_version,
            LOCUS_SCHEMA_VERSION
        );
    }
    require_text("goal", &request.goal)?;
    for (name, length) in [
        ("seeds", request.seeds.len()),
        ("obligations", request.obligations.len()),
        ("non-goals", request.non_goals.len()),
        ("assumptions", request.assumptions.len()),
        ("acquisitions", request.acquisitions.len()),
        ("supplied candidates", request.supplied_candidates.len()),
        ("discovery rules", request.discovery.len()),
        ("declared gaps", request.declared_gaps.len()),
    ] {
        if length > MAX_LOCUS_CASE_ITEMS {
            bail!("locus case {name} may not exceed {MAX_LOCUS_CASE_ITEMS} items");
        }
    }
    if request.seeds.is_empty() {
        bail!("locus case must declare at least one seed");
    }
    if request.obligations.is_empty() {
        bail!("locus case must declare at least one obligation");
    }
    if request.acquisitions.is_empty() {
        bail!("locus case must declare at least one acquisition");
    }
    if request.discovery.is_empty() {
        bail!("locus case must declare at least one discovery rule");
    }

    let mut global_ids = BTreeSet::new();
    for seed in &request.seeds {
        insert_id(&mut global_ids, "seed", &seed.id)?;
        require_label(&format!("seed {} label", seed.id), &seed.label)?;
        match &seed.selector {
            TraceSelector::Position { file, .. } if file.as_os_str().is_empty() => {
                bail!("seed {} position file must not be empty", seed.id)
            }
            TraceSelector::Position { line, character, .. }
                if *line == u32::MAX || *character == u32::MAX =>
            {
                bail!("seed {} position exceeds the output coordinate range", seed.id)
            }
            TraceSelector::Symbol { query, .. } => {
                require_label(&format!("seed {} symbol query", seed.id), query)?;
            }
            TraceSelector::Position { .. } => {}
        }
    }
    let seed_ids = request.seeds.iter().map(|seed| seed.id.as_str()).collect::<BTreeSet<_>>();

    for obligation in &request.obligations {
        insert_id(&mut global_ids, "obligation", &obligation.id)?;
        require_text(&format!("obligation {} statement", obligation.id), &obligation.statement)?;
        if obligation.acquisition_ids.is_empty() && obligation.gap_ids.is_empty() {
            bail!("obligation {} must reference an acquisition or declared gap", obligation.id);
        }
        require_unique_references(
            &format!("obligation {} acquisition", obligation.id),
            &obligation.acquisition_ids,
        )?;
        require_unique_references(
            &format!("obligation {} gap", obligation.id),
            &obligation.gap_ids,
        )?;
    }
    let requirement_count = request
        .obligations
        .iter()
        .map(|obligation| obligation.acquisition_ids.len() + obligation.gap_ids.len())
        .sum::<usize>();
    if requirement_count > MAX_LOCUS_REQUIREMENTS {
        bail!("locus case obligation matrix may not exceed {MAX_LOCUS_REQUIREMENTS} requirements");
    }

    for statement in request.non_goals.iter().chain(&request.assumptions) {
        insert_id(&mut global_ids, "statement", &statement.id)?;
        require_text(&format!("statement {} text", statement.id), &statement.statement)?;
    }

    for acquisition in &request.acquisitions {
        insert_id(&mut global_ids, "acquisition", &acquisition.id)?;
        if !seed_ids.contains(acquisition.seed_id.as_str()) {
            bail!("acquisition {} references unknown seed {}", acquisition.id, acquisition.seed_id);
        }
        validate_operation(&acquisition.id, &acquisition.operation)?;
        if acquisition.accept_no_call_item
            && !matches!(
                &acquisition.operation,
                LocusOperation::IncomingCalls { .. } | LocusOperation::OutgoingCalls { .. }
            )
        {
            bail!("acquisition {} accepts no-call-item for a non-call operation", acquisition.id);
        }
    }
    let acquisition_ids =
        request.acquisitions.iter().map(|item| item.id.as_str()).collect::<BTreeSet<_>>();
    for obligation in &request.obligations {
        for acquisition_id in &obligation.acquisition_ids {
            if request
                .acquisitions
                .iter()
                .find(|acquisition| acquisition.id == *acquisition_id)
                .is_some_and(|acquisition| !acquisition.required)
            {
                bail!(
                    "obligation {} may not depend on optional acquisition {acquisition_id}",
                    obligation.id
                );
            }
        }
    }

    let mut supplied_positions = BTreeSet::new();
    for candidate in &request.supplied_candidates {
        insert_id(&mut global_ids, "supplied candidate", &candidate.id)?;
        require_label(&format!("supplied candidate {} label", candidate.id), &candidate.label)?;
        if candidate.position.file.as_os_str().is_empty() {
            bail!("supplied candidate {} file must not be empty", candidate.id);
        }
        if candidate.position.line == u32::MAX || candidate.position.character == u32::MAX {
            bail!(
                "supplied candidate {} position exceeds the output coordinate range",
                candidate.id
            );
        }
        if !supplied_positions.insert((
            candidate.position.file.clone(),
            candidate.position.line,
            candidate.position.character,
        )) {
            bail!("supplied candidates repeat an exact source position");
        }
    }
    let supplied_ids = request
        .supplied_candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect::<BTreeSet<_>>();

    for rule in &request.discovery {
        insert_id(&mut global_ids, "discovery rule", &rule.id)?;
        validate_discovery_rule(rule, &seed_ids, &supplied_ids, &request.acquisitions)?;
    }

    let obligation_ids =
        request.obligations.iter().map(|item| item.id.as_str()).collect::<BTreeSet<_>>();
    for gap in &request.declared_gaps {
        insert_id(&mut global_ids, "declared gap", &gap.id)?;
        require_text(&format!("declared gap {} statement", gap.id), &gap.statement)?;
        if gap.obligation_ids.is_empty() {
            bail!("declared gap {} must reference at least one obligation", gap.id);
        }
        require_unique_references(
            &format!("declared gap {} obligation", gap.id),
            &gap.obligation_ids,
        )?;
        for obligation_id in &gap.obligation_ids {
            if !obligation_ids.contains(obligation_id.as_str()) {
                bail!("declared gap {} references unknown obligation {obligation_id}", gap.id);
            }
        }
    }
    let gap_ids = request.declared_gaps.iter().map(|gap| gap.id.as_str()).collect::<BTreeSet<_>>();

    for obligation in &request.obligations {
        for acquisition_id in &obligation.acquisition_ids {
            if !acquisition_ids.contains(acquisition_id.as_str()) {
                bail!(
                    "obligation {} references unknown acquisition {acquisition_id}",
                    obligation.id
                );
            }
        }
        for gap_id in &obligation.gap_ids {
            if !gap_ids.contains(gap_id.as_str()) {
                bail!("obligation {} references unknown declared gap {gap_id}", obligation.id);
            }
            let gap = request
                .declared_gaps
                .iter()
                .find(|gap| gap.id == *gap_id)
                .expect("validated declared gap id exists");
            if !gap.obligation_ids.contains(&obligation.id) {
                bail!(
                    "obligation {} and declared gap {} must reference each other",
                    obligation.id,
                    gap.id
                );
            }
        }
    }
    for gap in &request.declared_gaps {
        for obligation_id in &gap.obligation_ids {
            let obligation = request
                .obligations
                .iter()
                .find(|obligation| obligation.id == *obligation_id)
                .expect("validated obligation id exists");
            if !obligation.gap_ids.contains(&gap.id) {
                bail!(
                    "declared gap {} and obligation {} must reference each other",
                    gap.id,
                    obligation.id
                );
            }
        }
    }
    Ok(())
}

pub fn evaluate(request: LocusRequest, mut capture: LocusCapture) -> Result<LocusResult> {
    validate_request(&request)?;
    validate_capture(&request, &capture)?;
    normalize_capture(&request, &mut capture);

    let blocks = blocks(&capture);
    let gaps = request.declared_gaps.iter().map(result_gap).collect::<Vec<_>>();
    let obligations = request
        .obligations
        .iter()
        .map(|obligation| {
            result_obligation(obligation, &capture.acquisitions, &request.declared_gaps)
        })
        .collect::<Vec<_>>();
    let (mut candidates, discovery_receipts) = discover_candidates(&request, &capture)?;
    attach_candidate_obligations(
        &mut candidates,
        &request.obligations,
        &capture.acquisitions,
        &capture.evidence,
        &request.declared_gaps,
    );

    let investigation_required =
        request.acquisitions.iter().any(|spec| {
            spec.required
                && capture.acquisitions.iter().find(|result| result.id == spec.id).is_some_and(
                    |result| acquisition_open(&result.state, result.accept_no_call_item),
                )
        }) || request.declared_gaps.iter().any(|gap| gap.required)
            || discovery_receipts.iter().any(|receipt| {
                matches!(
                    &receipt.state,
                    LocusDiscoveryState::IncompleteEvidence | LocusDiscoveryState::Cut { .. }
                )
            });
    let status = if !blocks.is_empty() {
        LocusStatus::Blocked
    } else if investigation_required {
        LocusStatus::InvestigationRequired
    } else if candidates.is_empty() {
        LocusStatus::NoCandidate
    } else {
        LocusStatus::EvidenceReady
    };

    Ok(LocusResult {
        goal: request.goal,
        status,
        blocks,
        seeds: capture.seeds,
        obligations,
        assumptions: request.assumptions,
        non_goals: request.non_goals,
        acquisitions: capture.acquisitions,
        evidence: capture.evidence,
        candidates,
        discovery_receipts,
        gaps,
        freshness: capture.freshness,
        fingerprint: capture.fingerprint,
        timing: capture.timing,
    })
}

fn require_text(name: &str, value: &str) -> Result<()> {
    require_bounded_text(name, value, MAX_LOCUS_TEXT_BYTES)
}

fn require_label(name: &str, value: &str) -> Result<()> {
    require_bounded_text(name, value, MAX_LOCUS_LABEL_BYTES)
}

fn require_id(name: &str, value: &str) -> Result<()> {
    require_bounded_text(name, value, MAX_LOCUS_ID_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        bail!("{name} must use only ASCII letters, digits, '-', '_', '.', or ':'");
    }
    Ok(())
}

fn require_bounded_text(name: &str, value: &str, maximum: usize) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    if value.len() > maximum {
        bail!("{name} may not exceed {maximum} UTF-8 bytes");
    }
    Ok(())
}

fn insert_id(ids: &mut BTreeSet<String>, kind: &str, id: &str) -> Result<()> {
    require_id(&format!("{kind} id"), id)?;
    if !ids.insert(id.to_owned()) {
        bail!("locus case id {id} is duplicated");
    }
    Ok(())
}

fn require_unique_references(name: &str, values: &[String]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for value in values {
        require_id(name, value)?;
        if !unique.insert(value) {
            bail!("{name} reference {value} is duplicated");
        }
    }
    Ok(())
}

fn validate_operation(acquisition_id: &str, operation: &LocusOperation) -> Result<()> {
    match operation {
        LocusOperation::Definition { max_results }
        | LocusOperation::References { max_results, .. }
        | LocusOperation::Implementations { max_results } => {
            if *max_results == 0 || *max_results > MAX_LOCUS_LOCATIONS {
                bail!(
                    "acquisition {acquisition_id} max_results must be between 1 and {MAX_LOCUS_LOCATIONS}"
                );
            }
        }
        LocusOperation::IncomingCalls { limits } | LocusOperation::OutgoingCalls { limits } => {
            if limits.max_depth > MAX_TRACE_DEPTH {
                bail!("acquisition {acquisition_id} max_depth may not exceed {MAX_TRACE_DEPTH}");
            }
            if limits.max_nodes == 0 || limits.max_nodes > MAX_TRACE_NODES {
                bail!(
                    "acquisition {acquisition_id} max_nodes must be between 1 and {MAX_TRACE_NODES}"
                );
            }
        }
    }
    Ok(())
}

fn validate_discovery_rule(
    rule: &LocusDiscoveryRule,
    seed_ids: &BTreeSet<&str>,
    supplied_ids: &BTreeSet<&str>,
    acquisitions: &[LocusAcquisition],
) -> Result<()> {
    match &rule.strategy {
        LocusDiscoveryStrategy::SuppliedAnchors { candidate_ids } => {
            if candidate_ids.is_empty() {
                bail!("discovery rule {} must name at least one supplied candidate", rule.id);
            }
            require_unique_references(
                &format!("discovery rule {} supplied candidate", rule.id),
                candidate_ids,
            )?;
            for candidate_id in candidate_ids {
                if !supplied_ids.contains(candidate_id.as_str()) {
                    bail!(
                        "discovery rule {} references unknown supplied candidate {candidate_id}",
                        rule.id
                    );
                }
            }
        }
        LocusDiscoveryStrategy::SeedDefinitions { seed_ids: referenced }
        | LocusDiscoveryStrategy::ReturnedImplementations { seed_ids: referenced } => {
            validate_discovery_seeds(rule, referenced, seed_ids)?;
            let definitions =
                matches!(&rule.strategy, LocusDiscoveryStrategy::SeedDefinitions { .. });
            for seed_id in referenced {
                let exists = acquisitions.iter().any(|item| {
                    item.seed_id == *seed_id
                        && if definitions {
                            matches!(&item.operation, LocusOperation::Definition { .. })
                        } else {
                            matches!(&item.operation, LocusOperation::Implementations { .. })
                        }
                });
                if !exists {
                    bail!(
                        "discovery rule {} has no matching acquisition for seed {seed_id}",
                        rule.id
                    );
                }
            }
        }
        LocusDiscoveryStrategy::CallWitnessIntersection {
            seed_ids: referenced, direction, ..
        } => {
            validate_discovery_seeds(rule, referenced, seed_ids)?;
            if referenced.len() < 2 {
                bail!("discovery rule {} call intersection needs at least two seeds", rule.id);
            }
            for seed_id in referenced {
                let exists = acquisitions.iter().any(|item| {
                    item.seed_id == *seed_id
                        && matches!(
                            (*direction, &item.operation),
                            (TraceDirection::Callers, LocusOperation::IncomingCalls { .. })
                                | (TraceDirection::Callees, LocusOperation::OutgoingCalls { .. })
                        )
                });
                if !exists {
                    bail!(
                        "discovery rule {} has no matching call acquisition for seed {seed_id}",
                        rule.id
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_discovery_seeds(
    rule: &LocusDiscoveryRule,
    referenced: &[String],
    seed_ids: &BTreeSet<&str>,
) -> Result<()> {
    if referenced.is_empty() {
        bail!("discovery rule {} must name at least one seed", rule.id);
    }
    require_unique_references(&format!("discovery rule {} seed", rule.id), referenced)?;
    for seed_id in referenced {
        if !seed_ids.contains(seed_id.as_str()) {
            bail!("discovery rule {} references unknown seed {seed_id}", rule.id);
        }
    }
    Ok(())
}

fn validate_capture(request: &LocusRequest, capture: &LocusCapture) -> Result<()> {
    require_text("locus fingerprint", &capture.fingerprint)?;
    require_exact_ids(
        "seed result",
        request.seeds.iter().map(|seed| seed.id.as_str()),
        capture.seeds.iter().map(LocusSeedResult::seed_id),
    )?;
    require_exact_ids(
        "captured supplied candidate",
        request.supplied_candidates.iter().map(|item| item.id.as_str()),
        capture.supplied_candidates.iter().map(|item| item.request_id.as_str()),
    )?;

    let mut seed_anchors = BTreeMap::new();
    let mut ambiguity_candidate_count = 0usize;
    for requested in &request.seeds {
        let captured = capture
            .seeds
            .iter()
            .find(|seed| seed.seed_id() == requested.id)
            .expect("exact seed ids were validated");
        if seed_result_label(captured) != requested.label {
            bail!("seed result {} label does not match its request", requested.id);
        }
        if let (TraceSelector::Position { file, line, character }, Some(anchor)) =
            (&requested.selector, seed_result_anchor(captured))
        {
            if anchor.location.file != *file
                || anchor.location.line != *line + 1
                || anchor.location.character != *character + 1
            {
                bail!(
                    "resolved seed {} anchor does not match its requested position",
                    requested.id
                );
            }
        }
        match captured {
            LocusSeedResult::Resolved { anchor, .. } => {
                validate_anchor("resolved seed", anchor)?;
                seed_anchors.insert(requested.id.as_str(), anchor);
            }
            LocusSeedResult::AmbiguousCallItem { anchor, candidates, observed, .. } => {
                validate_anchor("ambiguous call-item seed", anchor)?;
                validate_ambiguity_candidates(
                    &format!("ambiguous call-item seed {}", requested.id),
                    candidates,
                    *observed,
                )?;
                ambiguity_candidate_count =
                    ambiguity_candidate_count.saturating_add(candidates.len());
                seed_anchors.insert(requested.id.as_str(), anchor);
            }
            LocusSeedResult::Ambiguous { candidates, observed, .. } => {
                validate_ambiguity_candidates(
                    &format!("ambiguous seed result {}", requested.id),
                    candidates,
                    *observed,
                )?;
                ambiguity_candidate_count =
                    ambiguity_candidate_count.saturating_add(candidates.len());
            }
            LocusSeedResult::Failed { reason, .. } => {
                require_text("seed failure reason", reason)?;
            }
            LocusSeedResult::NotFound { .. } => {}
        }
    }

    for requested in &request.supplied_candidates {
        let captured = capture
            .supplied_candidates
            .iter()
            .find(|candidate| candidate.request_id == requested.id)
            .expect("exact supplied candidate ids were validated");
        if captured.label != requested.label
            || captured.anchor.label != requested.label
            || captured.anchor.external
            || captured.anchor.location.file != requested.position.file
            || captured.anchor.location.line != requested.position.line + 1
            || captured.anchor.location.character != requested.position.character + 1
        {
            bail!("captured supplied candidate {} does not match its request", requested.id);
        }
        validate_anchor("captured supplied candidate", &captured.anchor)?;
    }

    if capture.evidence.len() > MAX_LOCUS_TOTAL_EVIDENCE {
        bail!("locus capture may not retain more than {MAX_LOCUS_TOTAL_EVIDENCE} evidence items");
    }
    require_exact_ids(
        "acquisition result",
        request.acquisitions.iter().map(|item| item.id.as_str()),
        capture.acquisitions.iter().map(|item| item.id.as_str()),
    )?;
    let mut evidence_by_id = BTreeMap::new();
    for evidence in &capture.evidence {
        if evidence_by_id.insert(evidence.id.as_str(), evidence).is_some() {
            bail!("locus evidence id {} is duplicated", evidence.id);
        }
        require_id("locus evidence id", &evidence.id)?;
        validate_anchor("locus evidence source", &evidence.source)?;
        validate_anchor("locus evidence target", &evidence.target)?;
        let mut call_sites = BTreeSet::new();
        for site in &evidence.call_sites {
            if site.file.as_os_str().is_empty() {
                bail!("locus evidence call site file must not be empty");
            }
            if !call_sites.insert(site) {
                bail!("locus evidence {} repeats a call site", evidence.id);
            }
        }
    }
    let acquisition_by_id = capture
        .acquisitions
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    for seed in &capture.seeds {
        if let LocusSeedResult::AmbiguousCallItem {
            seed_id,
            acquisition_id,
            candidates,
            observed,
            ..
        } = seed
        {
            let acquisition = acquisition_by_id.get(acquisition_id.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "ambiguous call-item seed {seed_id} references unknown acquisition {acquisition_id}"
                )
            })?;
            match &acquisition.state {
                LocusAcquisitionState::AmbiguousCallItem {
                    candidates: acquired_candidates,
                    observed: acquired_observed,
                } if acquisition.seed_id == *seed_id
                    && acquired_candidates == candidates
                    && acquired_observed == observed => {}
                _ => bail!(
                    "ambiguous call-item seed {seed_id} does not match acquisition {acquisition_id}"
                ),
            }
        }
    }
    let mut listed_evidence = BTreeSet::new();
    for spec in &request.acquisitions {
        let result = acquisition_by_id[spec.id.as_str()];
        if result.seed_id != spec.seed_id
            || result.required != spec.required
            || result.accept_no_call_item != spec.accept_no_call_item
            || result.operation != spec.operation
        {
            bail!("acquisition result {} does not match its request", spec.id);
        }
        require_unique_references(
            &format!("acquisition result {} evidence", result.id),
            &result.evidence_ids,
        )?;
        let retained = match &result.state {
            LocusAcquisitionState::CompleteWithinCapture { retained }
            | LocusAcquisitionState::Cut { retained, .. } => *retained,
            LocusAcquisitionState::Unsupported { .. }
            | LocusAcquisitionState::NoCallItem
            | LocusAcquisitionState::AmbiguousCallItem { .. }
            | LocusAcquisitionState::Failed { .. } => 0,
        };
        if retained != result.evidence_ids.len() {
            bail!(
                "acquisition result {} retained count does not match its evidence ids",
                result.id
            );
        }
        let direct_limit = match &spec.operation {
            LocusOperation::Definition { max_results }
            | LocusOperation::References { max_results, .. }
            | LocusOperation::Implementations { max_results } => Some(*max_results),
            LocusOperation::IncomingCalls { .. } | LocusOperation::OutgoingCalls { .. } => None,
        };
        if direct_limit.is_some_and(|maximum| retained > maximum) {
            bail!("acquisition result {} exceeds its requested result limit", result.id);
        }
        let call_operation = matches!(
            &result.operation,
            LocusOperation::IncomingCalls { .. } | LocusOperation::OutgoingCalls { .. }
        );
        match (call_operation, &result.state, &result.prepare) {
            (
                true,
                LocusAcquisitionState::CompleteWithinCapture { .. }
                | LocusAcquisitionState::Cut { .. },
                Some(prepare),
            ) => {
                validate_anchor("call prepare query anchor", &prepare.query_anchor)?;
                validate_anchor("prepared call root", &prepare.semantic_root)?;
                let seed_anchor = seed_anchors.get(result.seed_id.as_str()).ok_or_else(|| {
                    anyhow::anyhow!("call acquisition {} has no resolved query anchor", result.id)
                })?;
                if &prepare.query_anchor != *seed_anchor {
                    bail!(
                        "call acquisition {} prepare query does not match its resolved seed",
                        result.id
                    );
                }
            }
            (
                true,
                LocusAcquisitionState::CompleteWithinCapture { .. }
                | LocusAcquisitionState::Cut { .. },
                None,
            ) => bail!("call acquisition {} omitted its prepared semantic root", result.id),
            (true, _, Some(_)) => {
                bail!("non-retaining call acquisition {} reports a prepare receipt", result.id)
            }
            (false, _, Some(_)) => {
                bail!("direct acquisition {} reports a call prepare receipt", result.id)
            }
            (_, _, None) => {}
        }
        if let LocusAcquisitionState::AmbiguousCallItem { candidates, observed } = &result.state {
            validate_ambiguity_candidates(
                &format!("ambiguous call item {}", result.id),
                candidates,
                *observed,
            )?;
            ambiguity_candidate_count = ambiguity_candidate_count.saturating_add(candidates.len());
        }
        match &result.state {
            LocusAcquisitionState::Unsupported { reason }
            | LocusAcquisitionState::Failed { reason, .. } => {
                require_text("acquisition failure reason", reason)?;
            }
            _ => {}
        }
        if matches!(
            &result.state,
            LocusAcquisitionState::NoCallItem | LocusAcquisitionState::AmbiguousCallItem { .. }
        ) && !matches!(
            &result.operation,
            LocusOperation::IncomingCalls { .. } | LocusOperation::OutgoingCalls { .. }
        ) {
            bail!("non-call acquisition result {} has a call-item state", result.id);
        }
        if matches!(
            &result.state,
            LocusAcquisitionState::Unsupported { .. }
                | LocusAcquisitionState::NoCallItem
                | LocusAcquisitionState::AmbiguousCallItem { .. }
                | LocusAcquisitionState::Failed { .. }
        ) && !result.evidence_ids.is_empty()
        {
            bail!("non-retaining acquisition result {} lists evidence", result.id);
        }
        if matches!(&result.state, LocusAcquisitionState::Cut { cuts, .. } if cuts.is_empty()) {
            bail!("acquisition result {} is cut without a cut reason", result.id);
        }
        if let LocusAcquisitionState::Cut { cuts, .. } = &result.state {
            let mut reasons = BTreeSet::new();
            for cut in cuts {
                if !reasons.insert(cut.reason) {
                    bail!("acquisition result {} repeats a cut reason", result.id);
                }
                if matches!(cut.omission, super::protocol::LocusOmission::Known { count: 0 }) {
                    bail!("acquisition result {} has a zero known omission", result.id);
                }
            }
        }
        for evidence_id in &result.evidence_ids {
            if !listed_evidence.insert(evidence_id.as_str()) {
                bail!("locus evidence {evidence_id} is listed by multiple acquisitions");
            }
            let evidence = evidence_by_id.get(evidence_id.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "acquisition result {} references missing evidence {evidence_id}",
                    result.id
                )
            })?;
            if evidence.acquisition_id != result.id
                || evidence.seed_id != result.seed_id
                || evidence.relation != result.operation.relation()
            {
                bail!("locus evidence {evidence_id} provenance does not match its acquisition");
            }
            if matches!(
                &result.operation,
                LocusOperation::Definition { .. }
                    | LocusOperation::References { .. }
                    | LocusOperation::Implementations { .. }
            ) && match seed_anchors.get(result.seed_id.as_str()) {
                Some(anchor) => &evidence.source != *anchor,
                None => true,
            } {
                bail!("direct locus evidence {evidence_id} is not anchored at its seed");
            }
            let expected_capture = match &result.state {
                LocusAcquisitionState::CompleteWithinCapture { .. } => {
                    LocusEvidenceCapture::CompleteWithinCapture
                }
                LocusAcquisitionState::Cut { .. } => LocusEvidenceCapture::RetainedBeforeCut,
                _ => unreachable!("non-retaining acquisition listed evidence"),
            };
            if evidence.capture != expected_capture {
                bail!("locus evidence {evidence_id} has the wrong capture state");
            }
        }
        if call_operation && !result.evidence_ids.is_empty() {
            let root = result
                .prepare
                .as_ref()
                .expect("retaining call acquisition prepare receipt was validated");
            validate_call_connectivity(result, &capture.evidence, &root.semantic_root.location)?;
        }
    }
    if ambiguity_candidate_count > MAX_LOCUS_TOTAL_AMBIGUITY_CANDIDATES {
        bail!(
            "locus capture may not retain more than {MAX_LOCUS_TOTAL_AMBIGUITY_CANDIDATES} ambiguity candidates"
        );
    }
    if listed_evidence.len() != evidence_by_id.len() {
        bail!("locus capture contains evidence not listed by an acquisition");
    }
    let call_site_count = capture.evidence.iter().map(|item| item.call_sites.len()).sum::<usize>();
    if call_site_count > MAX_LOCUS_TOTAL_CALL_SITES {
        bail!("locus capture may not retain more than {MAX_LOCUS_TOTAL_CALL_SITES} call sites");
    }

    let mut files = BTreeSet::new();
    match &capture.freshness {
        LocusFreshness::Checked { files: checked } => {
            for file in checked {
                validate_captured_file(file.file.as_path(), &file.sha256)?;
                if !files.insert(&file.file) {
                    bail!("locus freshness contains duplicate file {}", file.file.display());
                }
            }
        }
        LocusFreshness::ChangedObservedInput { unchanged_files, changed_files } => {
            if changed_files.is_empty() {
                bail!("changed-observed-input freshness requires a changed file");
            }
            for file in unchanged_files {
                validate_captured_file(file.file.as_path(), &file.sha256)?;
                if !files.insert(&file.file) {
                    bail!("locus freshness contains duplicate file {}", file.file.display());
                }
            }
            for file in changed_files {
                if file.file.as_os_str().is_empty() || !valid_sha256(&file.before_sha256) {
                    bail!("changed locus freshness entry has invalid file or before hash");
                }
                match &file.after {
                    LocusRecheckValue::Sha256 { sha256 } if !valid_sha256(sha256) => {
                        bail!("changed locus freshness entry has an invalid after hash");
                    }
                    LocusRecheckValue::Unavailable { reason } => {
                        require_text("freshness recheck failure reason", reason)?;
                    }
                    LocusRecheckValue::Sha256 { sha256 } if sha256 == &file.before_sha256 => {
                        bail!(
                            "changed locus freshness entry {} repeats its before hash",
                            file.file.display()
                        );
                    }
                    LocusRecheckValue::Sha256 { .. } => {}
                }
                if !files.insert(&file.file) {
                    bail!("locus freshness contains duplicate file {}", file.file.display());
                }
            }
        }
    }
    if files.len() > MAX_LOCUS_OBSERVED_FILES {
        bail!("locus freshness may not contain more than {MAX_LOCUS_OBSERVED_FILES} files");
    }
    if !seed_anchors.is_empty() && files.is_empty() {
        bail!("resolved locus seeds require at least one freshness-checked source file");
    }
    for anchor in seed_anchors.values().filter(|anchor| !anchor.external) {
        if !files.contains(&anchor.location.file) {
            bail!(
                "resolved locus seed file {} is missing from freshness receipts",
                anchor.location.file.display()
            );
        }
    }
    for candidate in &capture.supplied_candidates {
        if !files.contains(&candidate.anchor.location.file) {
            bail!(
                "supplied candidate file {} is missing from freshness receipts",
                candidate.anchor.location.file.display()
            );
        }
    }
    for root in capture
        .acquisitions
        .iter()
        .filter_map(|acquisition| acquisition.prepare.as_ref())
        .map(|prepare| &prepare.semantic_root)
        .filter(|root| !root.external)
    {
        if !files.contains(&root.location.file) {
            bail!(
                "prepared call root file {} is missing from freshness receipts",
                root.location.file.display()
            );
        }
    }
    Ok(())
}

fn validate_call_connectivity(
    result: &LocusAcquisitionResult,
    evidence: &[LocusEvidence],
    root: &TraceLocation,
) -> Result<()> {
    let direction = match &result.operation {
        LocusOperation::IncomingCalls { .. } => TraceDirection::Callers,
        LocusOperation::OutgoingCalls { .. } => TraceDirection::Callees,
        _ => return Ok(()),
    };
    let mut remaining =
        evidence.iter().filter(|item| item.acquisition_id == result.id).collect::<Vec<_>>();
    let mut reachable = BTreeSet::from([root.clone()]);
    while !remaining.is_empty() {
        let position = remaining.iter().position(|item| match direction {
            TraceDirection::Callers => reachable.contains(&item.target.location),
            TraceDirection::Callees => reachable.contains(&item.source.location),
        });
        let Some(position) = position else {
            bail!("call acquisition {} contains evidence disconnected from its seed", result.id);
        };
        let item = remaining.remove(position);
        reachable.insert(item.source.location.clone());
        reachable.insert(item.target.location.clone());
    }
    Ok(())
}

fn seed_result_label(seed: &LocusSeedResult) -> &str {
    match seed {
        LocusSeedResult::Resolved { label, .. }
        | LocusSeedResult::Ambiguous { label, .. }
        | LocusSeedResult::NotFound { label, .. }
        | LocusSeedResult::Failed { label, .. }
        | LocusSeedResult::AmbiguousCallItem { label, .. } => label,
    }
}

fn seed_result_anchor(seed: &LocusSeedResult) -> Option<&super::protocol::LocusAnchor> {
    match seed {
        LocusSeedResult::Resolved { anchor, .. }
        | LocusSeedResult::AmbiguousCallItem { anchor, .. } => Some(anchor),
        LocusSeedResult::Ambiguous { .. }
        | LocusSeedResult::NotFound { .. }
        | LocusSeedResult::Failed { .. } => None,
    }
}

fn validate_captured_file(file: &std::path::Path, sha256: &str) -> Result<()> {
    if file.as_os_str().is_empty() || !valid_sha256(sha256) {
        bail!("locus freshness entry has invalid file or SHA-256 hash");
    }
    Ok(())
}

fn validate_anchor(name: &str, anchor: &super::protocol::LocusAnchor) -> Result<()> {
    require_label(&format!("{name} label"), &anchor.label)?;
    if anchor.location.file.as_os_str().is_empty() {
        bail!("{name} file must not be empty");
    }
    Ok(())
}

fn validate_seed_candidate(candidate: &super::protocol::LocusSeedCandidate) -> Result<()> {
    require_label("locus ambiguity candidate label", &candidate.label)?;
    validate_anchor("locus ambiguity candidate anchor", &candidate.anchor)
}

fn validate_ambiguity_candidates(
    name: &str,
    candidates: &[super::protocol::LocusSeedCandidate],
    observed: usize,
) -> Result<()> {
    if observed < 2 || candidates.len() > MAX_LOCUS_CANDIDATES || observed < candidates.len() {
        bail!(
            "{name} must observe at least 2 candidates and retain at most {MAX_LOCUS_CANDIDATES}"
        );
    }
    let mut locations = BTreeSet::new();
    for candidate in candidates {
        validate_seed_candidate(candidate)?;
        if !locations.insert(&candidate.anchor.location) {
            bail!("{name} repeats ambiguity anchor {}", candidate.anchor.label);
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn normalize_capture(request: &LocusRequest, capture: &mut LocusCapture) {
    let seed_order = request
        .seeds
        .iter()
        .enumerate()
        .map(|(index, seed)| (seed.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    capture.seeds.sort_by_key(|seed| seed_order[seed.seed_id()]);
    for seed in &mut capture.seeds {
        match seed {
            LocusSeedResult::Ambiguous { candidates, .. }
            | LocusSeedResult::AmbiguousCallItem { candidates, .. } => {
                sort_seed_candidates(candidates);
            }
            LocusSeedResult::Resolved { .. }
            | LocusSeedResult::NotFound { .. }
            | LocusSeedResult::Failed { .. } => {}
        }
    }

    let acquisition_order = request
        .acquisitions
        .iter()
        .enumerate()
        .map(|(index, acquisition)| (acquisition.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    capture.acquisitions.sort_by_key(|item| acquisition_order[item.id.as_str()]);
    for acquisition in &mut capture.acquisitions {
        acquisition.evidence_ids.sort();
        match &mut acquisition.state {
            LocusAcquisitionState::AmbiguousCallItem { candidates, .. } => {
                sort_seed_candidates(candidates);
            }
            LocusAcquisitionState::Cut { cuts, .. } => {
                cuts.sort_by_key(|cut| cut.reason);
            }
            LocusAcquisitionState::CompleteWithinCapture { .. }
            | LocusAcquisitionState::Unsupported { .. }
            | LocusAcquisitionState::NoCallItem
            | LocusAcquisitionState::Failed { .. } => {}
        }
    }

    let candidate_order = request
        .supplied_candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    capture.supplied_candidates.sort_by_key(|item| candidate_order[item.request_id.as_str()]);
    for evidence in &mut capture.evidence {
        evidence.call_sites.sort();
        evidence.call_sites.dedup();
    }
    capture.evidence.sort_by(|left, right| left.id.cmp(&right.id));
    match &mut capture.freshness {
        LocusFreshness::Checked { files } => {
            files.sort_by(|left, right| left.file.cmp(&right.file))
        }
        LocusFreshness::ChangedObservedInput { unchanged_files, changed_files } => {
            unchanged_files.sort_by(|left, right| left.file.cmp(&right.file));
            changed_files.sort_by(|left, right| left.file.cmp(&right.file));
        }
    }
}

fn sort_seed_candidates(candidates: &mut [super::protocol::LocusSeedCandidate]) {
    candidates.sort_by(|left, right| {
        left.anchor
            .location
            .cmp(&right.anchor.location)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.anchor.label.cmp(&right.anchor.label))
            .then_with(|| left.anchor.external.cmp(&right.anchor.external))
    });
}

fn require_exact_ids<'a>(
    name: &str,
    expected: impl Iterator<Item = &'a str>,
    actual: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let expected = expected.collect::<BTreeSet<_>>();
    let actual_values = actual.collect::<Vec<_>>();
    let actual = actual_values.iter().copied().collect::<BTreeSet<_>>();
    if actual.len() != actual_values.len() {
        bail!("{name} ids contain a duplicate");
    }
    if expected != actual {
        bail!("{name} ids do not exactly match the case request");
    }
    Ok(())
}

fn blocks(capture: &LocusCapture) -> Vec<LocusBlock> {
    let mut blocks = Vec::new();
    let mut ambiguous_call_items = BTreeSet::new();
    for seed in &capture.seeds {
        match seed {
            LocusSeedResult::Resolved { .. } => {}
            LocusSeedResult::Ambiguous { seed_id, .. } => {
                blocks.push(LocusBlock::AmbiguousSeed { seed_id: seed_id.clone() });
            }
            LocusSeedResult::NotFound { seed_id, .. } => {
                blocks.push(LocusBlock::SeedNotFound { seed_id: seed_id.clone() });
            }
            LocusSeedResult::Failed { seed_id, session_integrity, .. } => {
                blocks.push(LocusBlock::SeedFailure {
                    seed_id: seed_id.clone(),
                    session_integrity: *session_integrity,
                });
            }
            LocusSeedResult::AmbiguousCallItem { acquisition_id, .. } => {
                if ambiguous_call_items.insert(acquisition_id.as_str()) {
                    blocks.push(LocusBlock::AmbiguousCallItem {
                        acquisition_id: acquisition_id.clone(),
                    });
                }
            }
        }
    }
    if matches!(capture.freshness, LocusFreshness::ChangedObservedInput { .. }) {
        blocks.push(LocusBlock::ChangedObservedInput);
    }
    for acquisition in &capture.acquisitions {
        if matches!(&acquisition.state, LocusAcquisitionState::AmbiguousCallItem { .. })
            && ambiguous_call_items.insert(acquisition.id.as_str())
        {
            blocks.push(LocusBlock::AmbiguousCallItem { acquisition_id: acquisition.id.clone() });
        }
        if matches!(
            &acquisition.state,
            LocusAcquisitionState::Failed { session_integrity: LocusSessionIntegrity::Lost, .. }
        ) {
            blocks.push(LocusBlock::LostSessionDuringAcquisition {
                acquisition_id: acquisition.id.clone(),
            });
        }
    }
    blocks
}

fn result_gap(gap: &LocusDeclaredGap) -> LocusGap {
    LocusGap {
        id: gap.id.clone(),
        family: gap.family,
        statement: gap.statement.clone(),
        required: gap.required,
        obligation_ids: gap.obligation_ids.clone(),
        provenance: LocusGapProvenance::DeclaredByCase,
    }
}

fn result_obligation(
    obligation: &LocusObligation,
    acquisitions: &[LocusAcquisitionResult],
    gaps: &[LocusDeclaredGap],
) -> LocusObligationResult {
    let open_acquisition = obligation.acquisition_ids.iter().any(|id| {
        acquisitions
            .iter()
            .find(|result| result.id == *id)
            .is_some_and(|result| acquisition_open(&result.state, result.accept_no_call_item))
    });
    let open_gap = obligation
        .gap_ids
        .iter()
        .any(|id| gaps.iter().find(|gap| gap.id == *id).is_some_and(|gap| gap.required));
    LocusObligationResult {
        id: obligation.id.clone(),
        statement: obligation.statement.clone(),
        state: if open_acquisition || open_gap {
            LocusObligationState::Open
        } else {
            LocusObligationState::ClosedWithinDeclaredCapture
        },
        acquisition_ids: obligation.acquisition_ids.clone(),
        gap_ids: obligation.gap_ids.clone(),
    }
}

fn acquisition_open(state: &LocusAcquisitionState, accept_no_call_item: bool) -> bool {
    match state {
        LocusAcquisitionState::NoCallItem => !accept_no_call_item,
        LocusAcquisitionState::Cut { .. }
        | LocusAcquisitionState::Unsupported { .. }
        | LocusAcquisitionState::AmbiguousCallItem { .. }
        | LocusAcquisitionState::Failed { .. } => true,
        LocusAcquisitionState::CompleteWithinCapture { .. } => false,
    }
}

#[derive(Clone)]
struct CandidateDraft {
    id: String,
    label: String,
    anchor: super::protocol::LocusAnchor,
    discovered_by: String,
}

fn discover_candidates(
    request: &LocusRequest,
    capture: &LocusCapture,
) -> Result<(Vec<LocusCandidate>, Vec<LocusDiscoveryReceipt>)> {
    let supplied = capture
        .supplied_candidates
        .iter()
        .map(|candidate| (candidate.request_id.as_str(), candidate))
        .collect::<BTreeMap<_, _>>();
    let acquisitions = capture
        .acquisitions
        .iter()
        .map(|result| (result.id.as_str(), result))
        .collect::<BTreeMap<_, _>>();
    let mut candidates = Vec::<CandidateDraft>::new();
    let mut candidate_by_location = BTreeMap::<TraceLocation, usize>::new();
    let mut candidate_ids = BTreeSet::<String>::new();
    let mut generated_id = 1usize;
    let mut receipts = Vec::new();
    let requirements_per_candidate = request
        .obligations
        .iter()
        .map(|obligation| obligation.acquisition_ids.len() + obligation.gap_ids.len())
        .sum::<usize>();
    let candidate_limit = MAX_LOCUS_CANDIDATES
        .min(MAX_LOCUS_MATRIX_CELLS.checked_div(requirements_per_candidate.max(1)).unwrap_or(0));

    for rule in &request.discovery {
        let (mut anchors, incomplete) = match &rule.strategy {
            LocusDiscoveryStrategy::SuppliedAnchors { candidate_ids } => (
                candidate_ids
                    .iter()
                    .map(|id| {
                        let captured = supplied[id.as_str()];
                        (captured.label.clone(), captured.anchor.clone(), Some(id.clone()))
                    })
                    .collect::<Vec<_>>(),
                false,
            ),
            LocusDiscoveryStrategy::SeedDefinitions { seed_ids } => (
                evidence_targets(
                    &capture.evidence,
                    seed_ids,
                    super::protocol::LocusRelationKind::Definition,
                ),
                discovery_incomplete(&request.acquisitions, &acquisitions, seed_ids, |operation| {
                    matches!(operation, LocusOperation::Definition { .. })
                }),
            ),
            LocusDiscoveryStrategy::ReturnedImplementations { seed_ids } => (
                evidence_targets(
                    &capture.evidence,
                    seed_ids,
                    super::protocol::LocusRelationKind::Implementation,
                ),
                discovery_incomplete(&request.acquisitions, &acquisitions, seed_ids, |operation| {
                    matches!(operation, LocusOperation::Implementations { .. })
                }),
            ),
            LocusDiscoveryStrategy::CallWitnessIntersection {
                seed_ids,
                direction,
                require_complete,
            } => call_intersection(
                &request.acquisitions,
                &acquisitions,
                &capture.evidence,
                seed_ids,
                *direction,
                *require_complete,
            ),
        };
        anchors.sort_by(|left, right| {
            left.1.location.cmp(&right.1.location).then_with(|| left.0.cmp(&right.0))
        });
        anchors.dedup_by(|left, right| left.1.location == right.1.location);

        let mut receipt_ids = Vec::new();
        let mut omitted_candidates = 0usize;
        for (label, anchor, supplied_id) in anchors {
            let index = match candidate_by_location.get(&anchor.location).copied() {
                Some(index) => index,
                None => {
                    if candidates.len() >= candidate_limit {
                        omitted_candidates += 1;
                        continue;
                    }
                    let id = match supplied_id {
                        Some(id) if !candidate_ids.contains(&id) => id,
                        _ => loop {
                            let id = format!("inspect-{generated_id:04}");
                            generated_id += 1;
                            if !candidate_ids.contains(&id) {
                                break id;
                            }
                        },
                    };
                    candidate_ids.insert(id.clone());
                    let index = candidates.len();
                    candidates.push(CandidateDraft {
                        id,
                        label,
                        anchor: anchor.clone(),
                        discovered_by: rule.id.clone(),
                    });
                    candidate_by_location.insert(anchor.location.clone(), index);
                    index
                }
            };
            receipt_ids.push(candidates[index].id.clone());
        }
        receipt_ids.sort();
        receipt_ids.dedup();
        receipts.push(LocusDiscoveryReceipt {
            rule_id: rule.id.clone(),
            strategy: rule.strategy.clone(),
            state: if omitted_candidates > 0 {
                LocusDiscoveryState::Cut {
                    omission: super::protocol::LocusOmission::Known { count: omitted_candidates },
                }
            } else if incomplete {
                LocusDiscoveryState::IncompleteEvidence
            } else if receipt_ids.is_empty() {
                LocusDiscoveryState::NoMatch
            } else {
                LocusDiscoveryState::Applied
            },
            candidate_ids: receipt_ids,
        });
    }

    Ok((
        candidates
            .into_iter()
            .map(|candidate| LocusCandidate {
                id: candidate.id,
                label: candidate.label,
                anchor: candidate.anchor,
                discovered_by: candidate.discovered_by,
                obligations: Vec::new(),
            })
            .collect(),
        receipts,
    ))
}

fn evidence_targets(
    evidence: &[LocusEvidence],
    seed_ids: &[String],
    relation: super::protocol::LocusRelationKind,
) -> Vec<(String, super::protocol::LocusAnchor, Option<String>)> {
    evidence
        .iter()
        .filter(|item| item.relation == relation && seed_ids.contains(&item.seed_id))
        .map(|item| (item.target.label.clone(), item.target.clone(), None))
        .collect()
}

fn discovery_incomplete(
    specs: &[LocusAcquisition],
    results: &BTreeMap<&str, &LocusAcquisitionResult>,
    seed_ids: &[String],
    operation_matches: impl Fn(&LocusOperation) -> bool,
) -> bool {
    specs
        .iter()
        .filter(|spec| seed_ids.contains(&spec.seed_id) && operation_matches(&spec.operation))
        .any(|spec| !acquisition_complete_for_discovery(&results[spec.id.as_str()].state))
}

fn acquisition_complete_for_discovery(state: &LocusAcquisitionState) -> bool {
    matches!(state, LocusAcquisitionState::CompleteWithinCapture { .. })
}

fn call_intersection(
    specs: &[LocusAcquisition],
    results: &BTreeMap<&str, &LocusAcquisitionResult>,
    evidence: &[LocusEvidence],
    seed_ids: &[String],
    direction: TraceDirection,
    require_complete: bool,
) -> (Vec<(String, super::protocol::LocusAnchor, Option<String>)>, bool) {
    let relation = match direction {
        TraceDirection::Callers => super::protocol::LocusRelationKind::IncomingCall,
        TraceDirection::Callees => super::protocol::LocusRelationKind::OutgoingCall,
    };
    let matches_spec = |spec: &LocusAcquisition, seed_id: &str| {
        spec.seed_id == seed_id
            && matches!(
                (direction, &spec.operation),
                (TraceDirection::Callers, LocusOperation::IncomingCalls { .. })
                    | (TraceDirection::Callees, LocusOperation::OutgoingCalls { .. })
            )
    };
    let incomplete = seed_ids.iter().any(|seed_id| {
        specs
            .iter()
            .filter(|spec| matches_spec(spec, seed_id))
            .any(|spec| !acquisition_complete_for_discovery(&results[spec.id.as_str()].state))
    });
    if require_complete && incomplete {
        return (Vec::new(), true);
    }

    let mut intersection: Option<BTreeMap<TraceLocation, super::protocol::LocusAnchor>> = None;
    for seed_id in seed_ids {
        let acquisition_ids = specs
            .iter()
            .filter(|spec| matches_spec(spec, seed_id))
            .map(|spec| spec.id.as_str())
            .collect::<BTreeSet<_>>();
        let anchors = evidence
            .iter()
            .filter(|item| {
                item.seed_id == *seed_id
                    && item.relation == relation
                    && acquisition_ids.contains(item.acquisition_id.as_str())
            })
            .map(|item| match direction {
                TraceDirection::Callers => item.source.clone(),
                TraceDirection::Callees => item.target.clone(),
            })
            .map(|anchor| (anchor.location.clone(), anchor))
            .collect::<BTreeMap<_, _>>();
        intersection = Some(match intersection {
            None => anchors,
            Some(previous) => previous
                .into_iter()
                .filter(|(location, _)| anchors.contains_key(location))
                .collect(),
        });
    }
    let anchors = intersection
        .unwrap_or_default()
        .into_values()
        .map(|anchor| (anchor.label.clone(), anchor, None))
        .collect();
    (anchors, incomplete)
}

fn attach_candidate_obligations(
    candidates: &mut [LocusCandidate],
    obligations: &[LocusObligation],
    acquisitions: &[LocusAcquisitionResult],
    evidence: &[LocusEvidence],
    gaps: &[LocusDeclaredGap],
) {
    for candidate in candidates {
        candidate.obligations = obligations
            .iter()
            .map(|obligation| LocusCandidateObligation {
                obligation_id: obligation.id.clone(),
                requirements: obligation
                    .acquisition_ids
                    .iter()
                    .map(|acquisition_id| {
                        let acquisition = acquisitions
                            .iter()
                            .find(|result| result.id == *acquisition_id)
                            .expect("validated acquisition id exists");
                        let mut witnesses = evidence
                            .iter()
                            .filter(|item| {
                                item.acquisition_id == *acquisition_id
                                    && (item.source.location == candidate.anchor.location
                                        || item.target.location == candidate.anchor.location)
                            })
                            .map(|item| item.id.clone())
                            .collect::<Vec<_>>();
                        witnesses.sort();
                        let observed = witnesses.len();
                        witnesses.truncate(MAX_LOCUS_WITNESSES_PER_REQUIREMENT);
                        let state = if witnesses.is_empty() {
                            match &acquisition.state {
                                LocusAcquisitionState::CompleteWithinCapture { .. } => {
                                    LocusRequirementState::NotObservedWithinCompleteAcquisition
                                }
                                LocusAcquisitionState::Cut { .. } => LocusRequirementState::OpenCut,
                                LocusAcquisitionState::Unsupported { .. } => {
                                    LocusRequirementState::OpenUnsupported
                                }
                                LocusAcquisitionState::NoCallItem => {
                                    if acquisition.accept_no_call_item {
                                        LocusRequirementState::AcceptedNoCallItem
                                    } else {
                                        LocusRequirementState::OpenNoCallItem
                                    }
                                }
                                LocusAcquisitionState::AmbiguousCallItem { .. } => {
                                    LocusRequirementState::OpenFailed
                                }
                                LocusAcquisitionState::Failed { .. } => {
                                    LocusRequirementState::OpenFailed
                                }
                            }
                        } else {
                            match &acquisition.state {
                                LocusAcquisitionState::Cut { .. } => {
                                    LocusRequirementState::WitnessedBeforeCut {
                                        evidence_ids: witnesses,
                                        observed,
                                    }
                                }
                                _ => LocusRequirementState::Witnessed {
                                    evidence_ids: witnesses,
                                    observed,
                                },
                            }
                        };
                        LocusRequirementResult { acquisition_id: acquisition_id.clone(), state }
                    })
                    .chain(obligation.gap_ids.iter().filter_map(|gap_id| {
                        gaps.iter().find(|gap| gap.id == *gap_id).filter(|gap| gap.required).map(
                            |_| LocusRequirementResult {
                                acquisition_id: format!("declared-gap:{gap_id}"),
                                state: LocusRequirementState::OpenDeclaredGap {
                                    gap_id: gap_id.clone(),
                                },
                            },
                        )
                    }))
                    .collect(),
            })
            .collect();
    }
}
