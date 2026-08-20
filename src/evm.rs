use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use sha3::{Digest, Keccak256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    task::JoinSet,
    time::{Duration, sleep},
};

use crate::{FOMO_SOLANA_NETWORK_ID, USDC_MINT};

const RELAY_API_BASE: &str = "https://api.relay.link";
const RELAY_SOLANA_CHAIN_ID: u64 = 792_703_809;
const RELAY_PAGE_SIZE: usize = 50;
const MAX_PROFILE_RELAY_PAGES: usize = 20;
const DIRECT_RELAY_PAGE_SIZE: usize = 50;
const MAX_DIRECT_RELAY_PAGES: usize = 20;
const MAX_CANDIDATES: usize = 10;
const CANDIDATE_BATCH_SIZE: usize = 3;
const MAX_RELAY_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvmResolution {
    Resolved(String),
    Unavailable(EvmUnavailableReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvmUnavailableReason {
    NoEligibleSwap,
    NoMatchingRelayRequest,
    ConflictingRecipients,
    FomoSwapsUnavailable,
    RelayUnavailable,
    RelayHistoryIncomplete,
}

impl EvmUnavailableReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::NoEligibleSwap => "No completed Solana-to-EVM Relay swap was found",
            Self::NoMatchingRelayRequest => "No matching successful Relay request was found",
            Self::ConflictingRecipients => "Relay evidence did not identify one EVM wallet",
            Self::FomoSwapsUnavailable => "FOMO swap history could not be read",
            Self::RelayUnavailable => "Relay history could not be read",
            Self::RelayHistoryIncomplete => "Relay history was too large to verify completely",
        }
    }
}

pub(crate) async fn resolve(client: &Client, wallet: &str, swaps: &Value) -> Result<EvmResolution> {
    let candidates = swap_candidates(swaps);
    if candidates.is_empty() {
        return Ok(EvmResolution::Unavailable(
            EvmUnavailableReason::NoEligibleSwap,
        ));
    }

    let candidates = candidates
        .into_iter()
        .take(MAX_CANDIDATES)
        .collect::<Vec<_>>();
    let first = &candidates[0];
    let Some(requests) = fetch_requests(client, wallet, first).await? else {
        return Ok(EvmResolution::Unavailable(
            EvmUnavailableReason::RelayHistoryIncomplete,
        ));
    };
    if let Some(resolution) = match_candidate(wallet, first, &requests) {
        return Ok(resolution);
    }

    for batch in candidates[1..].chunks(CANDIDATE_BATCH_SIZE) {
        let mut requests = JoinSet::new();
        for (index, candidate) in batch.iter().cloned().enumerate() {
            let client = client.clone();
            let wallet = wallet.to_owned();
            requests.spawn(async move {
                let requests = fetch_requests(&client, &wallet, &candidate).await?;
                Ok::<_, anyhow::Error>((index, candidate, requests))
            });
        }

        let mut pages = Vec::with_capacity(batch.len());
        while let Some(result) = requests.join_next().await {
            pages.push(result.context("Relay candidate lookup task failed")??);
        }
        pages.sort_by_key(|(index, _, _)| *index);
        for (_, candidate, requests) in pages {
            let Some(requests) = requests else {
                return Ok(EvmResolution::Unavailable(
                    EvmUnavailableReason::RelayHistoryIncomplete,
                ));
            };
            if let Some(resolution) = match_candidate(wallet, &candidate, &requests) {
                return Ok(resolution);
            }
        }
    }

    Ok(EvmResolution::Unavailable(
        EvmUnavailableReason::NoMatchingRelayRequest,
    ))
}

pub(crate) async fn resolve_wallet(client: &Client, wallet: &str) -> Result<EvmResolution> {
    let mut requests = Vec::new();
    let mut continuation = None;

    for _ in 0..MAX_DIRECT_RELAY_PAGES {
        let page = fetch_wallet_requests(client, wallet, continuation.as_deref()).await?;
        requests.extend(page.requests);
        if match_wallet_requests(wallet, &requests)
            == EvmResolution::Unavailable(EvmUnavailableReason::ConflictingRecipients)
        {
            return Ok(EvmResolution::Unavailable(
                EvmUnavailableReason::ConflictingRecipients,
            ));
        }

        let Some(next) = page.continuation.filter(|value| !value.is_empty()) else {
            return Ok(match_wallet_requests(wallet, &requests));
        };
        continuation = Some(next);
    }

    Ok(EvmResolution::Unavailable(
        EvmUnavailableReason::RelayHistoryIncomplete,
    ))
}

fn match_wallet_requests(wallet: &str, requests: &[RelayRequest]) -> EvmResolution {
    let recipients = requests
        .iter()
        .filter_map(|request| verified_transfer(wallet, request))
        .map(|transfer| transfer.recipient)
        .collect::<HashSet<_>>();

    match recipients.len() {
        0 => EvmResolution::Unavailable(EvmUnavailableReason::NoMatchingRelayRequest),
        1 => EvmResolution::Resolved(recipients.into_iter().next().unwrap()),
        _ => EvmResolution::Unavailable(EvmUnavailableReason::ConflictingRecipients),
    }
}

async fn fetch_requests(
    client: &Client,
    wallet: &str,
    candidate: &SwapCandidate,
) -> Result<Option<Vec<RelayRequest>>> {
    let mut url = Url::parse(RELAY_API_BASE)?.join("/requests/v2")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("user", wallet)
            .append_pair("originChainId", &RELAY_SOLANA_CHAIN_ID.to_string())
            .append_pair(
                "destinationChainId",
                &candidate.output_network_id.to_string(),
            )
            .append_pair(
                "startTimestamp",
                &candidate.timestamp.saturating_sub(1).to_string(),
            )
            .append_pair(
                "endTimestamp",
                &candidate.timestamp.saturating_add(1).to_string(),
            )
            .append_pair("referrer", "fomo")
            .append_pair("limit", &RELAY_PAGE_SIZE.to_string())
            .append_pair("sortBy", "createdAt")
            .append_pair("sortDirection", "desc");
    }

    fetch_request_pages(client, url, MAX_PROFILE_RELAY_PAGES).await
}

async fn fetch_request_pages(
    client: &Client,
    url: Url,
    max_pages: usize,
) -> Result<Option<Vec<RelayRequest>>> {
    let mut requests = Vec::new();
    let mut continuation = None;

    for _ in 0..max_pages {
        let mut page_url = url.clone();
        if let Some(continuation) = continuation.as_deref() {
            page_url
                .query_pairs_mut()
                .append_pair("continuation", continuation);
        }

        let page = send_request(client, page_url).await?;
        requests.extend(page.requests);
        let Some(next) = page.continuation.filter(|value| !value.is_empty()) else {
            return Ok(Some(requests));
        };
        continuation = Some(next);
    }

    Ok(None)
}

async fn fetch_wallet_requests(
    client: &Client,
    wallet: &str,
    continuation: Option<&str>,
) -> Result<RelayRequestsPage> {
    let mut url = Url::parse(RELAY_API_BASE)?.join("/requests/v2")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("user", wallet)
            .append_pair("originChainId", &RELAY_SOLANA_CHAIN_ID.to_string())
            .append_pair("referrer", "fomo")
            .append_pair("status", "success")
            .append_pair("limit", &DIRECT_RELAY_PAGE_SIZE.to_string())
            .append_pair("sortBy", "createdAt")
            .append_pair("sortDirection", "desc");
        if let Some(continuation) = continuation {
            query.append_pair("continuation", continuation);
        }
    }

    send_request(client, url).await
}

async fn send_request(client: &Client, url: Url) -> Result<RelayRequestsPage> {
    for attempt in 0..MAX_RELAY_ATTEMPTS {
        let response = match client
            .get(url.clone())
            .header("accept", "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) if attempt + 1 < MAX_RELAY_ATTEMPTS => {
                sleep(relay_retry_delay(attempt)).await;
                continue;
            }
            Err(error) => return Err(error).context("Relay request failed"),
        };
        let status = response.status();
        if (status.is_server_error() || status.as_u16() == 429) && attempt + 1 < MAX_RELAY_ATTEMPTS
        {
            sleep(relay_retry_delay(attempt)).await;
            continue;
        }
        let body = response
            .text()
            .await
            .context("could not read Relay response")?;
        if !status.is_success() {
            bail!("Relay returned HTTP {status}");
        }
        return serde_json::from_str(&body).context("Relay returned invalid JSON");
    }
    unreachable!("the retry loop always returns or errors")
}

fn relay_retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SwapCandidate {
    created_at: String,
    timestamp: i64,
    input_amount: u64,
    output_network_id: u64,
    output_token: String,
}

fn swap_candidates(value: &Value) -> Vec<SwapCandidate> {
    fn collect(value: &Value, candidates: &mut Vec<SwapCandidate>) {
        match value {
            Value::Array(items) => {
                for item in items {
                    collect(item, candidates);
                }
            }
            Value::Object(object) => {
                if let Some(candidate) = swap_candidate(object) {
                    candidates.push(candidate);
                }
                for child in object.values() {
                    collect(child, candidates);
                }
            }
            _ => {}
        }
    }

    let mut candidates = Vec::new();
    collect(value, &mut candidates);
    candidates.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    candidates.dedup();
    candidates
}

fn swap_candidate(object: &serde_json::Map<String, Value>) -> Option<SwapCandidate> {
    if object.get("provider")?.as_str()? != "RELAY"
        || object.get("outTradeId")?.as_str()?.is_empty()
        || json_u64(object.get("inNetworkId")?)? != FOMO_SOLANA_NETWORK_ID
        || object.get("inTokenAddress")?.as_str()? != USDC_MINT
    {
        return None;
    }

    let output_network_id = json_u64(object.get("outNetworkId")?)?;
    let output_token = object.get("outTokenAddress")?.as_str()?;
    if !is_supported_evm_network(output_network_id) || checksum_address(output_token).is_none() {
        return None;
    }

    let input_amount = json_u64(object.get("inAmount")?)?
        .checked_add(json_u64(object.get("platformFeeAmount")?)?)?;
    let created_at = object.get("createdAt")?.as_str()?;
    let timestamp = OffsetDateTime::parse(created_at, &Rfc3339)
        .ok()?
        .unix_timestamp();
    Some(SwapCandidate {
        created_at: created_at.to_owned(),
        timestamp,
        input_amount,
        output_network_id,
        output_token: output_token.to_owned(),
    })
}

fn match_candidate(
    wallet: &str,
    candidate: &SwapCandidate,
    requests: &[RelayRequest],
) -> Option<EvmResolution> {
    let recipients = requests
        .iter()
        .filter_map(|request| matching_recipient(wallet, candidate, request))
        .collect::<HashSet<_>>();
    match recipients.len() {
        0 => None,
        1 => recipients.into_iter().next().map(EvmResolution::Resolved),
        _ => Some(EvmResolution::Unavailable(
            EvmUnavailableReason::ConflictingRecipients,
        )),
    }
}

fn matching_recipient(
    wallet: &str,
    candidate: &SwapCandidate,
    request: &RelayRequest,
) -> Option<String> {
    if request.created_at.as_deref()? != candidate.created_at {
        return None;
    }

    let transfer = verified_transfer(wallet, request)?;
    if transfer.input_amount != candidate.input_amount
        || transfer.output_network_id != candidate.output_network_id
        || !same_address(&transfer.output_token, &candidate.output_token)
    {
        return None;
    }

    Some(transfer.recipient)
}

struct VerifiedTransfer {
    recipient: String,
    input_amount: u64,
    output_network_id: u64,
    output_token: String,
}

fn verified_transfer(wallet: &str, request: &RelayRequest) -> Option<VerifiedTransfer> {
    if request.status.as_deref()? != "success"
        || request.user.as_deref()? != wallet
        || request.referrer.as_deref()? != "fomo"
    {
        return None;
    }

    let recipient = checksum_address(request.recipient.as_deref()?)?;
    let data = request.data.as_ref()?;
    let metadata = data.metadata.as_ref()?;
    if metadata.sender.as_deref()? != wallet
        || !same_address(metadata.recipient.as_deref()?, &recipient)
    {
        return None;
    }

    let input = metadata.currency_in.as_ref()?;
    let output = metadata.currency_out.as_ref()?;
    let output_token = checksum_address(&output.currency.address)?;
    if input.currency.chain_id != RELAY_SOLANA_CHAIN_ID
        || input.currency.address != USDC_MINT
        || !is_supported_evm_network(output.currency.chain_id)
    {
        return None;
    }
    let input_amount = input.amount.parse().ok()?;

    let credited = data.out_txs.iter().any(|transaction| {
        transaction.chain_id == output.currency.chain_id
            && transaction.status == "success"
            && transaction.state_changes.iter().any(|state_change| {
                same_address(&state_change.address, &recipient)
                    && same_address(
                        &state_change.change.data.token_address,
                        &output.currency.address,
                    )
                    && state_change
                        .change
                        .balance_diff
                        .parse::<i128>()
                        .is_ok_and(|amount| amount > 0)
            })
    });
    credited.then_some(VerifiedTransfer {
        recipient,
        input_amount,
        output_network_id: output.currency.chain_id,
        output_token,
    })
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
}

fn is_supported_evm_network(network_id: u64) -> bool {
    matches!(network_id, 1 | 56 | 143 | 4663 | 8453)
}

fn checksum_address(address: &str) -> Option<String> {
    let bytes = address.as_bytes();
    if bytes.len() != 42
        || !address.starts_with("0x")
        || !bytes[2..].iter().all(u8::is_ascii_hexdigit)
    {
        return None;
    }

    let lower = address[2..].to_ascii_lowercase();
    let hash = Keccak256::digest(lower.as_bytes());
    let mut checksum = String::with_capacity(42);
    checksum.push_str("0x");
    for (index, byte) in lower.bytes().enumerate() {
        let hash_nibble = if index % 2 == 0 {
            hash[index / 2] >> 4
        } else {
            hash[index / 2] & 0x0f
        };
        checksum.push(if byte.is_ascii_alphabetic() && hash_nibble >= 8 {
            (byte as char).to_ascii_uppercase()
        } else {
            byte as char
        });
    }
    Some(checksum)
}

fn same_address(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[derive(Debug, Deserialize)]
struct RelayRequestsPage {
    #[serde(default)]
    requests: Vec<RelayRequest>,
    continuation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayRequest {
    status: Option<String>,
    user: Option<String>,
    recipient: Option<String>,
    created_at: Option<String>,
    referrer: Option<String>,
    data: Option<RelayRequestData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayRequestData {
    metadata: Option<RelayMetadata>,
    #[serde(default)]
    out_txs: Vec<RelayTransaction>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayMetadata {
    sender: Option<String>,
    recipient: Option<String>,
    currency_in: Option<RelayAmount>,
    currency_out: Option<RelayAmount>,
}

#[derive(Debug, Deserialize)]
struct RelayAmount {
    currency: RelayCurrency,
    amount: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayCurrency {
    chain_id: u64,
    address: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayTransaction {
    chain_id: u64,
    status: String,
    #[serde(default)]
    state_changes: Vec<RelayStateChange>,
}

#[derive(Debug, Deserialize)]
struct RelayStateChange {
    address: String,
    change: RelayChange,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayChange {
    balance_diff: String,
    data: RelayChangeData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayChangeData {
    token_address: String,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::{Json, Router, extract::Query, routing::get};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    fn candidate() -> SwapCandidate {
        SwapCandidate {
            created_at: "2026-08-19T19:16:40.617Z".to_owned(),
            timestamp: OffsetDateTime::parse("2026-08-19T19:16:40.617Z", &Rfc3339)
                .unwrap()
                .unix_timestamp(),
            input_amount: 2_000_000,
            output_network_id: 4663,
            output_token: "0x6662060b16b61ba3f83bca6ccc796eb3acdf7777".to_owned(),
        }
    }

    fn request_value(recipient: &str, status: &str, amount: &str) -> Value {
        let wallet = "3cmts6s9Dhhbnw1ZMjkjnC6Ru3mRW6ckqzVPrax7mk78";
        json!({
            "status": status,
            "user": wallet,
            "recipient": recipient,
            "createdAt": "2026-08-19T19:16:40.617Z",
            "referrer": "fomo",
            "data": {
                "metadata": {
                    "sender": wallet,
                    "recipient": recipient,
                    "currencyIn": {
                        "currency": {
                            "chainId": RELAY_SOLANA_CHAIN_ID,
                            "address": USDC_MINT
                        },
                        "amount": amount
                    },
                    "currencyOut": {
                        "currency": {
                            "chainId": 4663,
                            "address": "0x6662060b16b61ba3f83bca6ccc796eb3acdf7777"
                        },
                        "amount": "100"
                    }
                },
                "outTxs": [{
                    "chainId": 4663,
                    "status": "success",
                    "stateChanges": [{
                        "address": recipient,
                        "change": {
                            "balanceDiff": "100",
                            "data": {
                                "tokenAddress": "0x6662060b16b61ba3f83bca6ccc796eb3acdf7777"
                            }
                        }
                    }]
                }]
            }
        })
    }

    fn request(recipient: &str, status: &str, amount: &str) -> RelayRequest {
        serde_json::from_value(request_value(recipient, status, amount)).unwrap()
    }

    #[test]
    fn extracts_only_completed_solana_to_evm_swaps() {
        let swaps = json!({
            "responseObject": [
                {
                    "provider": "RELAY",
                    "createdAt": "2026-08-19T19:16:40.617Z",
                    "inAmount": 1_990_000,
                    "platformFeeAmount": 10_000,
                    "inNetworkId": FOMO_SOLANA_NETWORK_ID,
                    "inTokenAddress": USDC_MINT,
                    "outNetworkId": 4663,
                    "outTokenAddress": "0x6662060b16b61ba3f83bca6ccc796eb3acdf7777",
                    "outTradeId": "trade-id"
                },
                {
                    "provider": "RELAY",
                    "createdAt": "2026-08-19T19:15:00.000Z",
                    "inAmount": 1_990_000,
                    "platformFeeAmount": 10_000,
                    "inNetworkId": FOMO_SOLANA_NETWORK_ID,
                    "inTokenAddress": USDC_MINT,
                    "outNetworkId": 4663,
                    "outTokenAddress": "0x6662060b16b61ba3f83bca6ccc796eb3acdf7777",
                    "outTradeId": null
                }
            ]
        });

        assert_eq!(swap_candidates(&swaps), vec![candidate()]);
    }

    #[test]
    fn requires_complete_matching_relay_evidence() {
        let recipient = "0x22CDAC3BBCA5Bc8472dfa81BC7a535f3d98ae9be";
        let wallet = "3cmts6s9Dhhbnw1ZMjkjnC6Ru3mRW6ckqzVPrax7mk78";

        assert_eq!(
            match_candidate(
                wallet,
                &candidate(),
                &[request(recipient, "success", "2000000")],
            ),
            Some(EvmResolution::Resolved(recipient.to_owned()))
        );
        assert_eq!(
            match_candidate(
                wallet,
                &candidate(),
                &[request(recipient, "success", "1999999")],
            ),
            None
        );
        assert_eq!(
            match_candidate(
                wallet,
                &candidate(),
                &[request(recipient, "refund", "2000000")],
            ),
            None
        );
    }

    #[test]
    fn rejects_conflicting_recipients() {
        let wallet = "3cmts6s9Dhhbnw1ZMjkjnC6Ru3mRW6ckqzVPrax7mk78";
        let requests = [
            request(
                "0x22CDAC3BBCA5Bc8472dfa81BC7a535f3d98ae9be",
                "success",
                "2000000",
            ),
            request(
                "0xB1e70177777125eBf46dFCA33e6Dc905c9533ba0",
                "success",
                "2000000",
            ),
        ];

        assert_eq!(
            match_candidate(wallet, &candidate(), &requests),
            Some(EvmResolution::Unavailable(
                EvmUnavailableReason::ConflictingRecipients
            ))
        );
    }

    #[test]
    fn resolves_a_wallet_directly_from_verified_relay_history() {
        let wallet = "3cmts6s9Dhhbnw1ZMjkjnC6Ru3mRW6ckqzVPrax7mk78";
        let recipient = "0x22CDAC3BBCA5Bc8472dfa81BC7a535f3d98ae9be";

        assert_eq!(
            match_wallet_requests(wallet, &[request(recipient, "success", "2000000")]),
            EvmResolution::Resolved(recipient.to_owned())
        );
        assert_eq!(
            match_wallet_requests(wallet, &[request(recipient, "refund", "2000000")]),
            EvmResolution::Unavailable(EvmUnavailableReason::NoMatchingRelayRequest)
        );
    }

    #[test]
    fn rejects_conflicting_direct_wallet_history() {
        let wallet = "3cmts6s9Dhhbnw1ZMjkjnC6Ru3mRW6ckqzVPrax7mk78";
        let requests = [
            request(
                "0x22CDAC3BBCA5Bc8472dfa81BC7a535f3d98ae9be",
                "success",
                "2000000",
            ),
            request(
                "0xB1e70177777125eBf46dFCA33e6Dc905c9533ba0",
                "success",
                "2000000",
            ),
        ];

        assert_eq!(
            match_wallet_requests(wallet, &requests),
            EvmResolution::Unavailable(EvmUnavailableReason::ConflictingRecipients)
        );
    }

    #[test]
    fn formats_evm_addresses_with_eip55_checksum() {
        assert_eq!(
            checksum_address("0xb1e70177777125ebf46dfca33e6dc905c9533ba0").as_deref(),
            Some("0xB1e70177777125eBf46dFCA33e6Dc905c9533ba0")
        );
        assert_eq!(
            checksum_address("0x52908400098527886e0f7030069857d2e4169ee7").as_deref(),
            Some("0x52908400098527886E0F7030069857D2E4169EE7")
        );
    }

    #[test]
    fn reads_relay_continuation_tokens() {
        let page: RelayRequestsPage = serde_json::from_value(json!({
            "requests": [],
            "continuation": "next-page"
        }))
        .unwrap();

        assert_eq!(page.continuation.as_deref(), Some("next-page"));
    }

    #[tokio::test]
    async fn reads_every_profile_relay_page_before_resolving() {
        async fn relay_page(Query(query): Query<HashMap<String, String>>) -> Json<Value> {
            let second_page = query
                .get("continuation")
                .is_some_and(|value| value == "next");
            let recipient = if second_page {
                "0xB1e70177777125eBf46dFCA33e6Dc905c9533ba0"
            } else {
                "0x22CDAC3BBCA5Bc8472dfa81BC7a535f3d98ae9be"
            };
            let mut response = json!({
                "requests": [request_value(recipient, "success", "2000000")]
            });
            if !second_page {
                response["continuation"] = json!("next");
            }
            Json(response)
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/requests/v2", get(relay_page));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = Url::parse(&format!("http://{address}/requests/v2")).unwrap();
        let requests = fetch_request_pages(&Client::new(), url, 2)
            .await
            .unwrap()
            .unwrap();
        server.abort();

        assert_eq!(
            match_candidate(
                "3cmts6s9Dhhbnw1ZMjkjnC6Ru3mRW6ckqzVPrax7mk78",
                &candidate(),
                &requests,
            ),
            Some(EvmResolution::Unavailable(
                EvmUnavailableReason::ConflictingRecipients
            ))
        );
    }
}
