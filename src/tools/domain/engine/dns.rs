use hickory_resolver::proto::rr::RData;
use hickory_resolver::TokioResolver;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delegation {
    Delegated(Vec<String>),
    Undelegated,
    Unknown,
}

pub async fn delegated(resolver: &TokioResolver, domain: &str) -> Delegation {
    let fqdn = format!("{}.", domain.trim_end_matches('.'));
    match resolver.ns_lookup(fqdn.as_str()).await {
        Ok(lookup) => {
            let nameservers = lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    RData::NS(ns) => Some(ns.to_string().trim_end_matches('.').to_lowercase()),
                    _ => None,
                })
                .filter(|ns| !ns.is_empty())
                .collect::<Vec<_>>();

            if nameservers.is_empty() {
                Delegation::Undelegated
            } else {
                Delegation::Delegated(nameservers)
            }
        }
        Err(err) if err.is_no_records_found() || err.is_nx_domain() => Delegation::Undelegated,
        Err(_) => Delegation::Unknown,
    }
}
