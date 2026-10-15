use std::collections::BTreeMap;

use price_feed::{
    Asset, Clock, ConnectionError, FeedId, FeedKind, FeedRegistry, PriceSource, SourceObservation,
};
use serde_json::Value;

use super::http_client;
use crate::config::MyceliaSignalConfig;

// Answers a signed envelope whose `canonical` field is the string the source
// signed: `v1|PRICE|PAIR|value|currency|decimals|sources|method|observed_at|nonce`.
// The price is read out of that string rather than a convenience field, so the
// value this source reports is the one that was signed.

/// Field positions in the canonical.
const FIELD_KIND: usize = 1;
const FIELD_PAIR: usize = 2;
const FIELD_VALUE: usize = 3;
const FIELD_OBSERVED_AT: usize = 8;
const FIELD_COUNT: usize = 10;

pub struct MyceliaSignal {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    /// Every feed this source prices, all quoted in USD.
    routes: BTreeMap<FeedId, Route>,
}

#[derive(Clone, Copy)]
struct Route {
    path: &'static str,
    /// The pair the canonical must name for this route.
    pair: &'static str,
    decimals: u32,
}

impl MyceliaSignal {
    /// What an operator, and a persisted freeze, name this source by. It must
    /// never change, or a frozen source comes back unfrozen.
    pub const NAME: &'static str = "myceliasignal";

    pub fn new(config: &MyceliaSignalConfig) -> Result<Self, reqwest::Error> {
        let routes = FeedRegistry::default()
            .feeds()
            .filter(|definition| {
                definition.kind == FeedKind::Direct && definition.quote == Asset::Usd
            })
            .filter_map(|definition| {
                let (path, pair) = route(definition.base)?;
                let route = Route {
                    path,
                    pair,
                    decimals: definition.decimals,
                };
                Some((definition.id, route))
            })
            .collect();

        Ok(Self {
            client: http_client()?,
            api_key: config.api_key.clone(),
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            routes,
        })
    }

    pub fn feeds(&self) -> impl Iterator<Item = FeedId> + '_ {
        self.routes.keys().copied()
    }

    async fn fetch(&self, route: Route) -> Result<Value, ConnectionError> {
        let mut request = self.client.get(format!("{}{}", self.base_url, route.path));
        if let Some(key) = &self.api_key {
            request = request.header("x-api-key", key);
        }
        let response = request
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| ConnectionError::RequestFailure)?;
        response
            .json()
            .await
            .map_err(|_| ConnectionError::MalformedResponse)
    }
}

impl PriceSource for MyceliaSignal {
    async fn poll(&self, feed: FeedId) -> Result<SourceObservation, ConnectionError> {
        let route = *self
            .routes
            .get(&feed)
            .ok_or(ConnectionError::RequestFailure)?;
        let body = self.fetch(route).await?;
        observation(&body, route, Clock::System.now())
    }
}

/// Prices LBTC as Bitcoin, the same one-to-one peg CoinGecko relies on, and
/// EURx and DePix by the currency each is pegged to. Every price is a median
/// across venues outside Liquid, so it is independent of SideSwap's order books
/// and of the Blockstream price API; and its USDT/USD is one SideSwap can
/// convert with when CoinGecko is frozen or not configured.
fn route(asset: Asset) -> Option<(&'static str, &'static str)> {
    match asset {
        Asset::Lbtc => Some(("/oracle/price/btc/usd", "BTCUSD")),
        Asset::Usdt => Some(("/oracle/price/usdt/usd", "USDTUSD")),
        Asset::Eurx => Some(("/oracle/price/eur/usd", "EURUSD")),
        Asset::DePix => Some(("/oracle/price/brl/usd", "BRLUSD")),
        Asset::Usdc => Some(("/oracle/price/usdc/usd", "USDCUSD")),
        Asset::Usd => None,
    }
}

fn observation(
    body: &Value,
    route: Route,
    received_at: u64,
) -> Result<SourceObservation, ConnectionError> {
    let canonical = body
        .get("canonical")
        .and_then(Value::as_str)
        .ok_or(ConnectionError::MalformedResponse)?;
    let field: Vec<&str> = canonical.split('|').collect();

    if field.len() < FIELD_COUNT || field[FIELD_KIND] != "PRICE" {
        return Err(ConnectionError::MalformedResponse);
    }
    // The canonical names the pair it priced. A response for another pair is
    // refused rather than assumed away: what is signed here is unrepudiable.
    if field[FIELD_PAIR] != route.pair {
        return Err(ConnectionError::MalformedResponse);
    }

    let price = exact_base_units(field[FIELD_VALUE], route.decimals)
        .ok_or(ConnectionError::MalformedResponse)?;
    let observed_at: u64 = field[FIELD_OBSERVED_AT]
        .parse()
        .map_err(|_| ConnectionError::MalformedResponse)?;

    Ok(SourceObservation::new(
        price,
        route.decimals,
        observed_at,
        received_at,
    ))
}

/// Scales a decimal string to `decimals` base units by digits rather than
/// through `f64`, unlike the shared `base_units`, so the value read out of the
/// canonical survives the conversion exactly. A quote carrying more precision
/// than the feed can hold is refused rather than rounded away.
fn exact_base_units(value: &str, decimals: u32) -> Option<u64> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let target = decimals as usize;
    if frac.len() > target {
        return None;
    }

    let mut digits = String::with_capacity(whole.len() + target);
    digits.push_str(whole);
    digits.push_str(frac);
    for _ in frac.len()..target {
        digits.push('0');
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use price_feed::{RejectionReason, registry::FEED_DECIMALS};
    use serde_json::json;

    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn body(canonical: &str) -> Value {
        json!({ "canonical": canonical, "signature": "…", "pubkey": "…" })
    }

    fn btc() -> Route {
        Route {
            path: "/oracle/price/btc/usd",
            pair: "BTCUSD",
            decimals: FEED_DECIMALS,
        }
    }

    /// A live BTCUSD canonical, 2026-09-28.
    const LIVE_BTC: &str =
        "v1|PRICE|BTCUSD|82786.21|USD|2|binance,bitfinex,coinbase|median|1700000000|843311";

    #[test]
    fn reads_the_price_out_of_the_canonical_at_the_feeds_decimals() {
        let observation = observation(&body(LIVE_BTC), btc(), NOW).unwrap();

        assert_eq!(observation.price, 8_278_621_000_000);
        assert_eq!(observation.decimals, FEED_DECIMALS);
        assert_eq!(observation.observed_at, 1_700_000_000);
        assert_eq!(observation.received_at, NOW);
    }

    /// The five Direct feeds as they were quoted on 2026-09-28.
    #[test]
    fn scales_every_live_feed_exactly() {
        let cases = [
            ("BTCUSD", "82786.21", 8_278_621_000_000_u64),
            ("USDTUSD", "0.99954", 99_954_000),
            ("EURUSD", "1.13778", 113_778_000),
            ("BRLUSD", "0.191837", 19_183_700),
            ("USDCUSD", "0.99982", 99_982_000),
        ];

        for (pair, value, expected) in cases {
            let canonical = format!("v1|PRICE|{pair}|{value}|USD|2|a,b|median|1700000000|1");
            let route = Route {
                path: "/p",
                pair,
                decimals: FEED_DECIMALS,
            };

            let observation = observation(&body(&canonical), route, NOW).unwrap();

            assert_eq!(observation.price, expected, "{pair}");
        }
    }

    #[test]
    fn refuses_a_canonical_for_another_pair() {
        let eur = "v1|PRICE|EURUSD|1.13778|USD|5|a,b|median|1700000000|1";

        assert_eq!(
            observation(&body(eur), btc(), NOW),
            Err(ConnectionError::MalformedResponse)
        );
    }

    #[test]
    fn refuses_a_canonical_that_prices_nothing() {
        // Another signed product, a truncated string, and no canonical at all.
        let sentiment = "v2|SENTIMENT|BTCUSD|-28.09|INDEX|2|a|x|1700000000|1";
        let truncated = "v1|PRICE|BTCUSD|82786.21";

        for canonical in [sentiment, truncated] {
            assert_eq!(
                observation(&body(canonical), btc(), NOW),
                Err(ConnectionError::MalformedResponse)
            );
        }
        assert_eq!(
            observation(&json!({ "price": 82786.21 }), btc(), NOW),
            Err(ConnectionError::MalformedResponse)
        );
    }

    #[test]
    fn reads_no_price_that_is_not_a_plain_decimal() {
        for value in ["", "1e5", "-1.0", "1.2.3", "8_2", " 1"] {
            assert_eq!(exact_base_units(value, FEED_DECIMALS), None, "{value}");
        }
    }

    /// Rounding a signed value away would change what the source attested to.
    #[test]
    fn refuses_more_precision_than_the_feed_can_hold() {
        assert_eq!(exact_base_units("1.234567890", FEED_DECIMALS), None);
        assert_eq!(
            exact_base_units("1.12345678", FEED_DECIMALS),
            Some(112_345_678)
        );
    }

    #[test]
    fn pads_a_short_fraction_and_a_bare_integer() {
        assert_eq!(exact_base_units("1.5", FEED_DECIMALS), Some(150_000_000));
        assert_eq!(exact_base_units("7", FEED_DECIMALS), Some(700_000_000));
        assert_eq!(exact_base_units("0.00000001", FEED_DECIMALS), Some(1));
    }

    /// `validate` is what rejects a zero price, so the parse must let it
    /// through rather than mask it as a malformed response.
    #[test]
    fn parses_a_zero_price_and_leaves_the_rejection_to_validate() {
        let zero = "v1|PRICE|BTCUSD|0|USD|2|a|median|1700000000|1";

        let observation = observation(&body(zero), btc(), NOW).unwrap();

        assert_eq!(observation.price, 0);
        assert_eq!(
            MyceliaSignal::validate(&observation, None),
            Err(RejectionReason::InvalidPrice)
        );
    }

    #[test]
    fn prices_the_five_direct_usd_feeds_and_no_others() {
        let priced: Vec<FeedId> = FeedRegistry::default()
            .feeds()
            .filter(|definition| {
                definition.kind == FeedKind::Direct && route(definition.base).is_some()
            })
            .map(|definition| definition.id)
            .collect();

        assert_eq!(priced, [0, 1, 2, 3, 7]);
    }

    /// CoinGecko lists neither EURx nor DePix; this source prices both by
    /// their pegs.
    #[test]
    fn prices_the_two_feeds_coingecko_cannot() {
        assert_eq!(route(Asset::Eurx).map(|(_, pair)| pair), Some("EURUSD"));
        assert_eq!(route(Asset::DePix).map(|(_, pair)| pair), Some("BRLUSD"));
    }
}
