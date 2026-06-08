mod dns;
mod rdap;
mod whois;

use std::{sync::Arc, time::Instant};

use anyhow::Result;
use hickory_resolver::TokioResolver;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::Semaphore, task::JoinSet};

#[derive(Clone)]
pub struct CheckClient {
    dns: TokioResolver,
    http: Client,
}

impl CheckClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            dns: TokioResolver::builder_tokio()?.build()?,
            http: Client::builder().user_agent("domain/0.1").build()?,
        })
    }

    pub async fn check_domain(&self, input: impl AsRef<str>) -> CheckResult {
        let start = Instant::now();
        let parsed = match CanonicalDomain::parse(input.as_ref()) {
            Ok(parsed) => parsed,
            Err(error) => return invalid_input_result(input.as_ref(), error, start),
        };
        let domain = parsed.as_str().to_owned();
        let tld = parsed.tld().to_owned();
        let mut attempts = Vec::new();

        let dns_start = Instant::now();
        let dns = dns_stage(&self.dns, &domain).await;
        attempts.push(attempt_from(
            Source::Dns,
            &dns,
            dns_start.elapsed().as_millis(),
        ));
        if let StageResult::Verdict { outcome, record } = dns {
            return finish(
                domain,
                Source::Dns,
                outcome,
                record,
                start.elapsed().as_millis(),
                attempts,
            );
        }

        let rdap_start = Instant::now();
        let rdap = rdap_stage(&self.http, &domain, &tld).await;
        attempts.push(attempt_from(
            Source::Rdap,
            &rdap,
            rdap_start.elapsed().as_millis(),
        ));
        if let StageResult::Verdict { outcome, record } = rdap {
            return finish(
                domain,
                Source::Rdap,
                outcome,
                record,
                start.elapsed().as_millis(),
                attempts,
            );
        }

        let whois_start = Instant::now();
        let outcome = whois::check(&domain, &tld).await;
        attempts.push(CheckAttempt::new(
            Source::Whois,
            AttemptStatus::from_verdict(outcome.verdict),
            outcome.evidence.clone(),
            whois_start.elapsed().as_millis(),
        ));
        finish(
            domain,
            Source::Whois,
            outcome,
            None,
            start.elapsed().as_millis(),
            attempts,
        )
    }

    pub async fn check_many(&self, domains: Vec<String>, limit: usize) -> Vec<CheckResult> {
        let limit = limit.max(1);
        let semaphore = Arc::new(Semaphore::new(limit));
        let mut jobs = JoinSet::new();

        for (index, domain) in domains.into_iter().enumerate() {
            let permit_pool = Arc::clone(&semaphore);
            let client = self.clone();
            jobs.spawn(async move {
                let permit = match permit_pool.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return (index, None),
                };
                let result = client.check_domain(domain).await;
                drop(permit);
                (index, Some(result))
            });
        }

        let mut ordered = Vec::new();
        while let Some(joined) = jobs.join_next().await {
            if let Ok((index, Some(result))) = joined {
                ordered.push((index, result));
            }
        }

        ordered.sort_by_key(|(index, _)| *index);
        ordered.into_iter().map(|(_, result)| result).collect()
    }
}

enum StageResult {
    Verdict {
        outcome: Outcome,
        record: Option<DomainRecord>,
    },
    Continue(String),
    Skip(String),
}

async fn dns_stage(resolver: &TokioResolver, domain: &str) -> StageResult {
    match dns::delegated(resolver, domain).await {
        dns::Delegation::Delegated(nameservers) => StageResult::Verdict {
            outcome: Outcome::taken("NS delegated"),
            record: Some(DomainRecord::from_nameservers(nameservers)),
        },
        dns::Delegation::Undelegated => StageResult::Continue("no NS delegation".to_owned()),
        dns::Delegation::Unknown => StageResult::Continue("NS lookup inconclusive".to_owned()),
    }
}

async fn rdap_stage(client: &Client, domain: &str, tld: &str) -> StageResult {
    let bootstrap = match rdap::load(client).await {
        rdap::BootstrapLoad::Ready(bootstrap) => bootstrap,
        rdap::BootstrapLoad::Failed(error) => {
            return StageResult::Continue(format!("RDAP bootstrap unavailable: {error}"));
        }
    };

    let endpoints = bootstrap.endpoints_for_domain(domain);
    if endpoints.is_empty() {
        return StageResult::Skip(format!("no RDAP endpoint for .{tld}"));
    }

    let checked = rdap::check(client, domain, &endpoints).await;
    match checked.outcome {
        Some(outcome) => StageResult::Verdict {
            outcome,
            record: checked.record,
        },
        None => StageResult::Continue(checked.evidence),
    }
}

fn attempt_from(source: Source, result: &StageResult, ms: u128) -> CheckAttempt {
    let (status, evidence) = match result {
        StageResult::Verdict { outcome, .. } => (
            AttemptStatus::from_verdict(outcome.verdict),
            outcome.evidence.clone(),
        ),
        StageResult::Continue(evidence) => (AttemptStatus::Inconclusive, evidence.clone()),
        StageResult::Skip(evidence) => (AttemptStatus::Skipped, evidence.clone()),
    };
    CheckAttempt::new(source, status, evidence, ms)
}

fn finish(
    domain: String,
    source: Source,
    outcome: Outcome,
    record: Option<DomainRecord>,
    ms: u128,
    attempts: Vec<CheckAttempt>,
) -> CheckResult {
    CheckResult::new(
        domain,
        outcome.verdict,
        source,
        outcome.evidence,
        ms,
        attempts,
        record,
    )
}

fn invalid_input_result(input: &str, error: DomainNameError, start: Instant) -> CheckResult {
    let domain = display_input(input);
    let evidence = format!("invalid domain syntax: {error}");
    let ms = start.elapsed().as_millis();
    let attempt = CheckAttempt::new(
        Source::Input,
        AttemptStatus::Inconclusive,
        evidence.clone(),
        ms,
    );
    CheckResult::new(
        domain,
        Verdict::Inconclusive,
        Source::Input,
        evidence,
        ms,
        vec![attempt],
        None,
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub domain: String,
    pub verdict: Verdict,
    pub source: Source,
    pub evidence: String,
    pub ms: u128,
    pub attempts: Vec<CheckAttempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<DomainRecord>,
}

impl CheckResult {
    fn new(
        domain: String,
        verdict: Verdict,
        source: Source,
        evidence: impl Into<String>,
        ms: u128,
        attempts: Vec<CheckAttempt>,
        record: Option<DomainRecord>,
    ) -> Self {
        Self {
            domain,
            verdict,
            source,
            evidence: evidence.into(),
            ms,
            attempts,
            record,
        }
    }

    pub fn disposition(&self) -> Option<Disposition> {
        self.record.as_ref().map(DomainRecord::disposition)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckAttempt {
    pub source: Source,
    pub status: AttemptStatus,
    pub evidence: String,
    pub ms: u128,
}

impl CheckAttempt {
    fn new(source: Source, status: AttemptStatus, evidence: impl Into<String>, ms: u128) -> Self {
        Self {
            source,
            status,
            evidence: evidence.into(),
            ms,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Available,
    Taken,
    Inconclusive,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Available => "available",
            Self::Taken => "taken",
            Self::Inconclusive => "inconclusive",
        };
        f.pad(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttemptStatus {
    Available,
    Taken,
    Inconclusive,
    Skipped,
}

impl AttemptStatus {
    fn from_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Available => Self::Available,
            Verdict::Taken => Self::Taken,
            Verdict::Inconclusive => Self::Inconclusive,
        }
    }
}

impl std::fmt::Display for AttemptStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Available => "available",
            Self::Taken => "taken",
            Self::Inconclusive => "inconclusive",
            Self::Skipped => "skipped",
        };
        f.pad(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Input,
    Dns,
    Rdap,
    Whois,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Input => "input",
            Self::Dns => "dns",
            Self::Rdap => "rdap",
            Self::Whois => "whois",
        };
        f.pad(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DomainRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registrar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub statuses: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub nameservers: Vec<String>,
}

impl DomainRecord {
    pub fn from_nameservers(nameservers: Vec<String>) -> Self {
        Self {
            registrar: None,
            created_on: None,
            expires_on: None,
            statuses: Vec::new(),
            nameservers,
        }
    }

    pub fn disposition(&self) -> Disposition {
        if let Some(stage) = expiry_stage(&self.statuses) {
            return Disposition::Expiring(stage);
        }
        if let Some(service) = parking_service(&self.nameservers) {
            return Disposition::Parked(service);
        }
        Disposition::Active
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    Active,
    Parked(ParkingService),
    Expiring(ExpiryStage),
}

impl std::fmt::Display for Disposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Parked(service) => write!(f, "parked · {service}"),
            Self::Expiring(stage) => write!(f, "expiring · {stage}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkingService {
    Sedo,
    Dan,
    Afternic,
    Bodis,
    ParkingCrew,
    HugeDomains,
    Uniregistry,
    BuyDomains,
}

impl std::fmt::Display for ParkingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Sedo => "Sedo",
            Self::Dan => "Dan",
            Self::Afternic => "Afternic",
            Self::Bodis => "Bodis",
            Self::ParkingCrew => "ParkingCrew",
            Self::HugeDomains => "HugeDomains",
            Self::Uniregistry => "Uniregistry",
            Self::BuyDomains => "BuyDomains",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpiryStage {
    Redemption,
    PendingDelete,
    Hold,
}

impl std::fmt::Display for ExpiryStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Redemption => "redemption period",
            Self::PendingDelete => "pending delete",
            Self::Hold => "on hold",
        };
        f.write_str(value)
    }
}

fn expiry_stage(statuses: &[String]) -> Option<ExpiryStage> {
    let has = |needle: &str| statuses.iter().any(|status| status.contains(needle));
    if has("pending delete") || has("pendingdelete") {
        Some(ExpiryStage::PendingDelete)
    } else if has("redemption") {
        Some(ExpiryStage::Redemption)
    } else if has("hold") {
        Some(ExpiryStage::Hold)
    } else {
        None
    }
}

fn parking_service(nameservers: &[String]) -> Option<ParkingService> {
    const PARKERS: &[(&str, ParkingService)] = &[
        ("sedoparking", ParkingService::Sedo),
        ("sedo.com", ParkingService::Sedo),
        ("dan.com", ParkingService::Dan),
        ("undeveloped.com", ParkingService::Dan),
        ("afternic", ParkingService::Afternic),
        ("above.com", ParkingService::Afternic),
        ("bodis.com", ParkingService::Bodis),
        ("parkingcrew", ParkingService::ParkingCrew),
        ("hugedomains", ParkingService::HugeDomains),
        ("uniregistrymarket", ParkingService::Uniregistry),
        ("uniregistry", ParkingService::Uniregistry),
        ("buydomains", ParkingService::BuyDomains),
        ("this-domain-for-sale", ParkingService::BuyDomains),
    ];

    nameservers.iter().find_map(|nameserver| {
        PARKERS
            .iter()
            .find(|(needle, _)| nameserver.contains(needle))
            .map(|(_, service)| *service)
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Outcome {
    verdict: Verdict,
    evidence: String,
}

impl Outcome {
    pub(crate) fn available(evidence: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Available,
            evidence: evidence.into(),
        }
    }

    pub(crate) fn taken(evidence: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Taken,
            evidence: evidence.into(),
        }
    }

    pub(crate) fn inconclusive(evidence: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Inconclusive,
            evidence: evidence.into(),
        }
    }
}

pub fn expand_domains<'a>(
    tokens: impl IntoIterator<Item = &'a str>,
    tlds: &[String],
) -> Vec<String> {
    let mut out = Vec::new();

    for token in tokens {
        let token = token.trim().to_lowercase();
        if token.is_empty() {
            continue;
        }

        if token.contains('.') {
            push_domain_candidate(&mut out, token);
        } else {
            for tld in tlds {
                if let Some(tld) = canonicalize_suffix(tld) {
                    push_domain_candidate(&mut out, format!("{token}.{tld}"));
                }
            }
        }
    }

    out
}

pub fn canonicalize_domain(input: &str) -> Result<String, DomainNameError> {
    CanonicalDomain::parse(input).map(|domain| domain.into_string())
}

pub fn canonicalize_query_token(input: &str) -> Result<String, DomainNameError> {
    let trimmed = input.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return Err(DomainNameError::Empty);
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(DomainNameError::InvalidSyntax);
    }

    if trimmed.contains('.') {
        return canonicalize_domain(trimmed);
    }

    let ascii = idna::domain_to_ascii_strict(trimmed)
        .map_err(|_| DomainNameError::InvalidSyntax)?
        .to_lowercase();
    if ascii.is_empty() || ascii.contains('.') {
        return Err(DomainNameError::InvalidSyntax);
    }

    Ok(ascii)
}

pub fn canonicalize_suffix(input: &str) -> Option<String> {
    let suffix = input.trim().trim_start_matches('.').trim_end_matches('.');
    if suffix.is_empty() {
        return None;
    }

    let domain = CanonicalDomain::parse(format!("example.{suffix}")).ok()?;
    domain
        .as_str()
        .strip_prefix("example.")
        .map(ToOwned::to_owned)
}

fn push_domain_candidate(out: &mut Vec<String>, candidate: String) {
    let normalized = canonicalize_domain(&candidate).unwrap_or_else(|_| display_input(&candidate));
    push_unique(out, normalized);
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !out.iter().any(|existing| existing == &value) {
        out.push(value);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CanonicalDomain {
    ascii: String,
    tld_start: usize,
}

impl CanonicalDomain {
    fn parse(input: impl AsRef<str>) -> Result<Self, DomainNameError> {
        let trimmed = input.as_ref().trim().trim_end_matches('.');
        if trimmed.is_empty() {
            return Err(DomainNameError::Empty);
        }
        if trimmed.chars().any(char::is_whitespace) {
            return Err(DomainNameError::InvalidSyntax);
        }

        let ascii = idna::domain_to_ascii_strict(trimmed)
            .map_err(|_| DomainNameError::InvalidSyntax)?
            .to_lowercase();
        let labels = ascii.split('.').collect::<Vec<_>>();
        if labels.len() < 2 {
            return Err(DomainNameError::MissingTld);
        }
        if labels.iter().any(|label| label.is_empty()) {
            return Err(DomainNameError::InvalidSyntax);
        }

        let tld_start = ascii
            .rfind('.')
            .map(|index| index + 1)
            .ok_or(DomainNameError::MissingTld)?;

        Ok(Self { ascii, tld_start })
    }

    fn as_str(&self) -> &str {
        &self.ascii
    }

    fn tld(&self) -> &str {
        &self.ascii[self.tld_start..]
    }

    fn into_string(self) -> String {
        self.ascii
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DomainNameError {
    #[error("empty input")]
    Empty,
    #[error("missing TLD")]
    MissingTld,
    #[error("not a valid DNS domain name")]
    InvalidSyntax,
}

fn display_input(input: &str) -> String {
    let cleaned = input.trim().trim_end_matches('.').to_lowercase();
    if cleaned.is_empty() {
        "<empty>".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_bare_names_across_tlds_and_dedupes_in_order() {
        let tlds = crate::tools::domain::DEFAULT_TLDS
            .iter()
            .map(|tld| (*tld).to_owned())
            .collect::<Vec<_>>();
        let domains = expand_domains(["ModKit", "modkit.ai", "modkit"].iter().copied(), &tlds);

        assert_eq!(
            domains,
            vec!["modkit.com", "modkit.ai", "modkit.io", "modkit.studio"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn normalizes_full_domains() {
        let tlds = vec!["com".to_owned()];
        let domains = expand_domains([" Example.COM. "].iter().copied(), &tlds);

        assert_eq!(domains, vec!["example.com".to_owned()]);
    }

    #[test]
    fn canonicalizes_idn_domains_to_ascii() {
        assert_eq!(
            canonicalize_domain("Bücher.example"),
            Ok("xn--bcher-kva.example".to_owned())
        );
    }

    #[test]
    fn canonicalizes_query_tokens_for_favorites() {
        assert_eq!(
            canonicalize_query_token(" Bücher "),
            Ok("xn--bcher-kva".to_owned())
        );
        assert_eq!(
            canonicalize_query_token(" Example.COM. "),
            Ok("example.com".to_owned())
        );
    }

    #[test]
    fn canonicalizes_suffixes_for_tld_sets() {
        assert_eq!(canonicalize_suffix(".COM."), Some("com".to_owned()));
        assert_eq!(canonicalize_suffix("co.uk"), Some("co.uk".to_owned()));
        assert_eq!(canonicalize_suffix("invalid suffix"), None);
    }

    #[test]
    fn expands_bare_idn_names_to_canonical_ascii() {
        let tlds = vec!["de".to_owned()];
        let domains = expand_domains(["Bücher"].iter().copied(), &tlds);

        assert_eq!(domains, vec!["xn--bcher-kva.de".to_owned()]);
    }

    #[test]
    fn rejects_invalid_domain_syntax() {
        assert_eq!(
            canonicalize_domain("example..com"),
            Err(DomainNameError::InvalidSyntax)
        );
        assert_eq!(
            canonicalize_domain("localhost"),
            Err(DomainNameError::MissingTld)
        );
    }

    #[tokio::test]
    async fn invalid_domain_returns_input_inconclusive_without_registry_chain() -> Result<()> {
        let client = CheckClient::new()?;
        let result = client.check_domain("example..com").await;

        assert_eq!(result.domain, "example..com");
        assert_eq!(result.verdict, Verdict::Inconclusive);
        assert_eq!(result.source, Source::Input);
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].source, Source::Input);
        assert!(result.evidence.contains("invalid domain syntax"));
        assert_eq!(result.record, None);

        Ok(())
    }

    #[test]
    fn classifies_parking_nameservers() {
        let parked = DomainRecord::from_nameservers(vec![
            "ns1.dan.com".to_owned(),
            "ns2.dan.com".to_owned(),
        ]);
        assert_eq!(
            parked.disposition(),
            Disposition::Parked(ParkingService::Dan)
        );

        let afternic = DomainRecord::from_nameservers(vec![
            "ns1.afternic.com".to_owned(),
            "ns2.afternic.com".to_owned(),
        ]);
        assert_eq!(
            afternic.disposition(),
            Disposition::Parked(ParkingService::Afternic)
        );

        let buydomains = DomainRecord::from_nameservers(vec![
            "ns.buydomains.com".to_owned(),
            "this-domain-for-sale.com".to_owned(),
        ]);
        assert_eq!(
            buydomains.disposition(),
            Disposition::Parked(ParkingService::BuyDomains)
        );

        let active = DomainRecord::from_nameservers(vec![
            "kip.ns.cloudflare.com".to_owned(),
            "dan.ns.cloudflare.com".to_owned(),
        ]);
        assert_eq!(active.disposition(), Disposition::Active);
    }

    #[test]
    fn registry_default_nameservers_are_not_parked() {
        let godaddy = DomainRecord::from_nameservers(vec![
            "ns01.domaincontrol.com".to_owned(),
            "ns02.domaincontrol.com".to_owned(),
        ]);

        assert_eq!(godaddy.disposition(), Disposition::Active);
    }

    #[test]
    fn expiry_statuses_outrank_parking() {
        let record = DomainRecord {
            registrar: None,
            created_on: None,
            expires_on: Some("2026-01-01".to_owned()),
            statuses: vec!["redemption period".to_owned()],
            nameservers: vec!["ns1.dan.com".to_owned()],
        };

        assert_eq!(
            record.disposition(),
            Disposition::Expiring(ExpiryStage::Redemption)
        );
    }

    #[test]
    fn maps_expiry_statuses_to_stages() {
        assert_eq!(
            expiry_stage(&["pending delete".to_owned()]),
            Some(ExpiryStage::PendingDelete)
        );
        assert_eq!(
            expiry_stage(&["client hold".to_owned()]),
            Some(ExpiryStage::Hold)
        );
        assert_eq!(
            expiry_stage(&["client transfer prohibited".to_owned()]),
            None
        );
    }
}
