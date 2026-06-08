use std::{collections::HashMap, sync::OnceLock, time::Duration};

use anyhow::{anyhow, Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
    time::timeout,
};

use super::Outcome;

const WHOIS_PORT: u16 = 43;
const WHOIS_TIMEOUT: Duration = Duration::from_secs(12);
const IANA_WHOIS: &str = "whois.iana.org";

const FREE_MARKERS: &[&str] = &[
    "no object found",
    "domain not found",
    "not found",
    "no match for",
    "no match",
    "no entries found",
    "status: available",
    "status: free",
    "not registered",
    "available for registration",
];

const TAKEN_MARKERS: &[&str] = &[
    "registrar:",
    "registrar whois",
    "registrar url",
    "creation date",
    "created:",
    "registry expiry",
    "expiry date",
    "name server",
    "nserver",
    "registrant",
    "status: ok",
    "status: active",
    "status: connect",
    "registry domain id",
    "domain id:",
    "domain status:",
];

static WHOIS_SERVER_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

pub async fn check(domain: &str, tld: &str) -> Outcome {
    let server = match server_for_tld(tld).await {
        WhoisServer::Found(server) => server,
        WhoisServer::NoReferral => {
            return Outcome::inconclusive("no WHOIS referral from IANA for TLD");
        }
        WhoisServer::LookupFailed(error) => {
            return Outcome::inconclusive(format!("IANA WHOIS referral unavailable: {error}"));
        }
    };

    let text = match whois_ask(&server, domain, WHOIS_TIMEOUT).await {
        Ok(text) => text,
        Err(error) => return Outcome::inconclusive(format!("whois {server}: {error:#}")),
    };

    if text.is_empty() {
        return Outcome::inconclusive(format!("whois {server}: no response"));
    }

    match classify_whois_marker(&text) {
        WhoisClassification::Free(marker) => {
            Outcome::available(format!("whois {server}: '{marker}'"))
        }
        WhoisClassification::Taken(marker) => Outcome::taken(format!("whois {server}: '{marker}'")),
        WhoisClassification::Ambiguous => {
            Outcome::inconclusive(format!("whois {server}: unrecognized record"))
        }
    }
}

async fn server_for_tld(tld: &str) -> WhoisServer {
    let key = tld.trim_start_matches('.').to_lowercase();
    let cache = WHOIS_SERVER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    {
        let cached = cache.lock().await;
        if let Some(server) = cached.get(&key) {
            return server
                .clone()
                .map(WhoisServer::Found)
                .unwrap_or(WhoisServer::NoReferral);
        }
    }

    let referral = match whois_ask(IANA_WHOIS, &key, WHOIS_TIMEOUT).await {
        Ok(referral) => referral,
        Err(error) => return WhoisServer::LookupFailed(format!("{error:#}")),
    };
    let server = parse_whois_referral(&referral);

    let mut cached = cache.lock().await;
    cached.insert(key, server.clone());
    server
        .map(WhoisServer::Found)
        .unwrap_or(WhoisServer::NoReferral)
}

enum WhoisServer {
    Found(String),
    NoReferral,
    LookupFailed(String),
}

async fn whois_ask(server: &str, query: &str, timeout_after: Duration) -> Result<String> {
    let mut stream = timeout(timeout_after, TcpStream::connect((server, WHOIS_PORT)))
        .await
        .context("connection timed out")?
        .with_context(|| format!("connect to {server}:43"))?;
    timeout(
        timeout_after,
        stream.write_all(format!("{query}\r\n").as_bytes()),
    )
    .await
    .context("write timed out")?
    .with_context(|| format!("write WHOIS query to {server}:43"))?;

    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match timeout(timeout_after, stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => data.extend_from_slice(&buffer[..n]),
            Ok(Err(error)) if data.is_empty() => {
                return Err(error).with_context(|| format!("read WHOIS response from {server}:43"));
            }
            Err(error) if data.is_empty() => return Err(anyhow!("read timed out: {error}")),
            Ok(Err(_)) | Err(_) => break,
        }
    }

    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn parse_whois_referral(referral: &str) -> Option<String> {
    referral.lines().find_map(|line| {
        let trimmed = line.trim();
        if !trimmed.to_lowercase().starts_with("whois:") {
            return None;
        }

        let colon = trimmed.find(':')?;
        let server = trimmed[colon + 1..].trim();
        (!server.is_empty()).then(|| server.to_owned())
    })
}

fn classify_whois_marker(text: &str) -> WhoisClassification {
    let normalized = collapse_whitespace(&text.to_lowercase());

    for marker in FREE_MARKERS {
        if normalized.contains(marker) {
            return WhoisClassification::Free(marker);
        }
    }

    for marker in TAKEN_MARKERS {
        if normalized.contains(marker) {
            return WhoisClassification::Taken(marker);
        }
    }

    WhoisClassification::Ambiguous
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WhoisClassification {
    Free(&'static str),
    Taken(&'static str),
    Ambiguous,
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_free_markers_before_taken_markers() {
        let response = "Registrar: example\nStatus: AVAILABLE\n";

        assert!(matches!(
            classify_whois_marker(response),
            WhoisClassification::Free(_)
        ));
    }

    #[test]
    fn classifies_taken_markers() {
        let response =
            "Domain Name: EXAMPLE.COM\nCreation Date: 1995-01-01\nName Server: A.IANA-SERVERS.NET";

        assert!(matches!(
            classify_whois_marker(response),
            WhoisClassification::Taken(_)
        ));
    }

    #[test]
    fn classifies_ambiguous_records_as_inconclusive() {
        let response = "Terms of use apply. Please try again later.";

        assert_eq!(
            classify_whois_marker(response),
            WhoisClassification::Ambiguous
        );
    }

    #[test]
    fn parses_referral_by_slicing_from_the_first_colon() {
        let referral = "domain: ai\nwhois: whois.example:43\nremarks: keep colons\n";

        assert_eq!(
            parse_whois_referral(referral),
            Some("whois.example:43".to_owned())
        );
    }

    #[test]
    fn reports_missing_referral_as_none() {
        let referral = "domain: invalid\nremarks: no referral published\n";

        assert_eq!(parse_whois_referral(referral), None);
    }
}
