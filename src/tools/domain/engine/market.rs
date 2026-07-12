use reqwest::Client;
use serde::Deserialize;

use super::{Listing, Marketplace, Price, SaleListing};

const FORSALE_BASE: &str = "https://www.afternic.com/forsale/";
const NEXT_DATA_OPEN: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;
const NEXT_DATA_CLOSE: &str = "</script>";

pub(crate) async fn probe(client: &Client, domain: &str) -> Option<Listing> {
    let url = format!("{FORSALE_BASE}{domain}");
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    parse_listing(&body, &url)
}

fn parse_listing(body: &str, url: &str) -> Option<Listing> {
    let next_data = extract_next_data(body)?;
    let state = serde_json::from_str::<NextData>(next_data).ok()?.props.initial_state;
    let domain = state.domain_data;

    if !domain.sellable {
        return Some(Listing::NotListed);
    }

    let currency = state.config.currency;
    let priced = |amount: u64| (amount > 0).then(|| Price { amount, currency: currency.clone() });

    Some(Listing::ForSale(SaleListing {
        marketplace: Marketplace::Afternic,
        buy_now: domain.buy_now_enabled.then(|| priced(domain.buy_now_price)).flatten(),
        minimum_offer: priced(domain.min_price),
        lease_to_own: domain.lease_to_own_enabled,
        url: url.to_owned(),
    }))
}

fn extract_next_data(body: &str) -> Option<&str> {
    let start = body.find(NEXT_DATA_OPEN)? + NEXT_DATA_OPEN.len();
    let rest = &body[start..];
    let end = rest.find(NEXT_DATA_CLOSE)?;
    Some(&rest[..end])
}

#[derive(Deserialize)]
struct NextData {
    props: Props,
}

#[derive(Deserialize)]
struct Props {
    #[serde(rename = "initialState")]
    initial_state: InitialState,
}

#[derive(Deserialize)]
struct InitialState {
    config: AfternicConfig,
    #[serde(rename = "domainData")]
    domain_data: DomainData,
}

#[derive(Deserialize)]
struct AfternicConfig {
    currency: String,
}

#[derive(Deserialize)]
struct DomainData {
    #[serde(default)]
    sellable: bool,
    #[serde(default, rename = "buyNowEnabled")]
    buy_now_enabled: bool,
    #[serde(default, rename = "buyNowPrice")]
    buy_now_price: u64,
    #[serde(default, rename = "minPrice")]
    min_price: u64,
    #[serde(default, rename = "leaseToOwnEnabled")]
    lease_to_own_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORSALE_FIXTURE: &str = r#"<!doctype html><html><head>
        <script id="__NEXT_DATA__" type="application/json">{"props":{"initialState":{"config":{"currency":"USD"},"domainData":{"name":"inkmod.com","sellable":true,"buyNowEnabled":true,"buyNowPrice":9895,"minPrice":2000,"leaseToOwnEnabled":false}}}}</script>
        </head><body><div id="__next"></div></body></html>"#;

    const NOT_FORSALE_FIXTURE: &str = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"initialState":{"config":{"currency":"USD"},"domainData":{"name":"google.com","sellable":false,"buyNowEnabled":false,"buyNowPrice":0,"minPrice":0,"leaseToOwnEnabled":false}}}}</script>"#;

    const MAKE_OFFER_FIXTURE: &str = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"initialState":{"config":{"currency":"EUR"},"domainData":{"name":"offer.com","sellable":true,"buyNowEnabled":false,"buyNowPrice":0,"minPrice":1500,"leaseToOwnEnabled":true}}}}</script>"#;

    #[test]
    fn parses_buy_now_listing_with_currency_from_config() {
        let listing = parse_listing(FORSALE_FIXTURE, "https://www.afternic.com/forsale/inkmod.com")
            .expect("listing parses");
        let Listing::ForSale(sale) = listing else {
            panic!("expected a for-sale listing");
        };

        assert_eq!(sale.marketplace, Marketplace::Afternic);
        assert_eq!(sale.buy_now, Some(Price { amount: 9895, currency: "USD".to_owned() }));
        assert_eq!(sale.minimum_offer, Some(Price { amount: 2000, currency: "USD".to_owned() }));
        assert!(!sale.lease_to_own);
        assert_eq!(sale.url, "https://www.afternic.com/forsale/inkmod.com");
        assert_eq!(sale.headline(), Some(&Price { amount: 9895, currency: "USD".to_owned() }));
        assert_eq!(sale.headline().unwrap().to_string(), "$9,895");
    }

    #[test]
    fn reports_not_listed_when_domain_is_not_sellable() {
        let listing =
            parse_listing(NOT_FORSALE_FIXTURE, "https://www.afternic.com/forsale/google.com")
                .expect("listing parses");

        assert_eq!(listing, Listing::NotListed);
    }

    #[test]
    fn reports_make_offer_listing_without_a_buy_now_price() {
        let listing =
            parse_listing(MAKE_OFFER_FIXTURE, "https://www.afternic.com/forsale/offer.com")
                .expect("listing parses");
        let Listing::ForSale(sale) = listing else {
            panic!("expected a for-sale listing");
        };

        assert_eq!(sale.buy_now, None);
        assert_eq!(sale.minimum_offer, Some(Price { amount: 1500, currency: "EUR".to_owned() }));
        assert!(sale.lease_to_own);
        assert_eq!(sale.headline(), Some(&Price { amount: 1500, currency: "EUR".to_owned() }));
        assert_eq!(sale.headline().unwrap().to_string(), "€1,500");
    }

    #[test]
    fn returns_none_when_next_data_is_absent() {
        assert_eq!(parse_listing("<html><body>no parking data</body></html>", "https://x"), None);
    }

    #[test]
    fn formats_prices_with_thousands_separators() {
        assert_eq!(Price { amount: 9895, currency: "USD".to_owned() }.to_string(), "$9,895");
        assert_eq!(Price { amount: 12, currency: "USD".to_owned() }.to_string(), "$12");
        assert_eq!(Price { amount: 1234567, currency: "EUR".to_owned() }.to_string(), "€1,234,567");
        assert_eq!(Price { amount: 1000, currency: "CHF".to_owned() }.to_string(), "1,000 CHF");
    }
}
