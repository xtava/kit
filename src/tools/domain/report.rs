use super::engine::{CheckAttempt, CheckResult, Disposition};

/// Headless text rendering — one aligned line per result, with a disposition tag and a trace line.
pub fn print_results(results: &[CheckResult]) {
    let domain_width =
        results.iter().map(|result| result.domain.len()).max().unwrap_or(6).max("domain".len());

    for result in results {
        println!(
            "{:<12} {:<domain_width$}  {} ({}, {}ms){}",
            result.verdict,
            result.domain,
            result.evidence,
            result.source,
            result.ms,
            disposition_tag(result)
        );
        if !result.attempts.is_empty() {
            println!("  trace: {}", format_attempts(&result.attempts));
        }
    }
}

fn disposition_tag(result: &CheckResult) -> String {
    match result.disposition() {
        Some(Disposition::Active) | None => String::new(),
        Some(disposition @ Disposition::Expiring(_)) => {
            match result.record.as_ref().and_then(|record| record.expires_on.as_deref()) {
                Some(expiration) => format!("  [{disposition}, exp {expiration}]"),
                None => format!("  [{disposition}]"),
            }
        }
        Some(disposition) => format!("  [{disposition}]"),
    }
}

fn format_attempts(attempts: &[CheckAttempt]) -> String {
    attempts
        .iter()
        .map(|attempt| {
            format!("{} {} {}ms {}", attempt.source, attempt.status, attempt.ms, attempt.evidence)
        })
        .collect::<Vec<_>>()
        .join(" -> ")
}
