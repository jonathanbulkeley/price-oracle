use std::{collections::BTreeMap, time::Duration};

use price_feed::{
    Asset, Clock, ConnectionError, FeedId, FeedKind, FeedRegistry, PriceSource, SourceObservation,
};
use serde_json::Value;

use crate::config::MyceliaConfig;

/// Answers a signed envelope whose `canonical` field is the string the source
/// signed: `v1|PRICE|PAIR|value|currency|decimals|sources|method|observed_at|nonce`.
/// The price is read out of that string rather than a convenience field, so the
/// value this source reports is the one that was signed.
const USER_AGENT: &str = concat!("high-storm/", env!("CARGO_PKG_VERSION"));

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Field positions in the canonical.
const FIELD_KIND: usize = 1;
const FIELD_PAIR: usize = 2;
const FIELD_VALUE: usize = 3;
const FIELD_OBSERVED_AT: usize = 8;
const FIELD_COUNT: usize = 10;

pub struct Mycelia {
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

impl Mycelia {
    pub fn new(config: &MyceliaConfig) -> Result<Self, reqwest::Error> {
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
            client: reqwest::Client::builder()
                .use_preconfigured_tls(super::tls())
                .user_agent(USER_AGENT)
                .timeout(REQUEST_TIMEOUT)
                .build()?,
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

impl PriceSource for Mycelia {
    async fn poll(&self, feed: FeedId) -> Result<SourceObservation, ConnectionError> {
        let route = *self
            .routes
            .get(&feed)
            .ok_or(ConnectionError::RequestFailure)?;
        let body = self.fetch(route).await?;
        observation(&body, route, Clock::System.now())
    }
}

/// Prices LBTC as Bitcoin, the same one-to-one peg CoinGecko relies on. EURx
/// and DePix are listed by no major price API, so each is priced by the
/// currency it is pegged to; without a source of this kind those two feeds have
/// none at all and answer `503` permanently.
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

    let price =
        base_units(field[FIELD_VALUE], route.decimals).ok_or(ConnectionError::MalformedResponse)?;
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
/// through `f64`, so the value read out of the canonical survives the
/// conversion exactly. A quote carrying more precision than the feed can hold
/// is refused rather than rounded away.
fn base_units(value: &str, decimals: u32) -> Option<u64> {
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
