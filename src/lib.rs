use std::{env, str::FromStr, sync::OnceLock, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client as RpcClient, Url};
use serde_json::{Value, json};
use solana_address_lookup_table_interface::{
    program as address_lookup_table_program, state::AddressLookupTable,
};
use solana_message::{AccountKeys, VersionedMessage, v0::LoadedAddresses};
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;
use tokio::time::{Instant, sleep};
use wreq::{Client as FomoClient, Method, StatusCode};
use wreq_util::Profile as WreqProfile;

mod evm;

pub use evm::{EvmResolution, EvmUnavailableReason};

pub const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

const FOMO_API_BASE: &str = "https://prod-api.fomo.family";
pub(crate) const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TRANSFER_AMOUNT: u64 = 2_000_000;
pub(crate) const FOMO_SOLANA_NETWORK_ID: u64 = 1_399_811_149;
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SUPPORTED_CHAINS: &str = "1,56,143,4663,8453,1399811149";
const MAX_FOMO_ATTEMPTS: u32 = 5;
const MAX_ERROR_BODY_CHARS: usize = 1_000;

pub struct ResolverConfig {
    bearer: String,
    rpc_url: String,
}

impl ResolverConfig {
    pub fn from_env(bearer: String) -> Result<Self> {
        let rpc_url = env::var("SOLANA_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_owned());
        Self::with_rpc(bearer, rpc_url)
    }

    pub fn with_rpc(bearer: String, rpc_url: String) -> Result<Self> {
        if bearer.trim().is_empty() {
            bail!("FOMO_BEARER_TOKEN cannot be empty");
        }

        Url::parse(&rpc_url).context("SOLANA_RPC_URL is not a valid URL")?;
        Ok(Self { bearer, rpc_url })
    }
}

fn shared_fomo_client() -> Result<FomoClient> {
    static CLIENT: OnceLock<FomoClient> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }

    let client = FomoClient::builder()
        .emulation(WreqProfile::Chrome148)
        .timeout(Duration::from_secs(30))
        .build()
        .context("could not build Chrome-emulated FOMO client")?;
    let _ = CLIENT.set(client);
    Ok(CLIENT.get().expect("FOMO client was initialized").clone())
}

fn shared_http_client() -> Result<RpcClient> {
    static CLIENT: OnceLock<RpcClient> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }

    let client = RpcClient::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("could not build HTTP client")?;
    let _ = CLIENT.set(client);
    Ok(CLIENT.get().expect("HTTP client was initialized").clone())
}

pub struct FomoWalletResolver {
    config: ResolverConfig,
    fomo: FomoClient,
    rpc: RpcClient,
    relay: RpcClient,
    api_base: Url,
    token_mint: Pubkey,
}

impl FomoWalletResolver {
    pub fn new(config: ResolverConfig) -> Result<Self> {
        let fomo = shared_fomo_client()?;
        let rpc = shared_http_client()?;

        Ok(Self {
            config,
            fomo,
            relay: rpc.clone(),
            rpc,
            api_base: Url::parse(FOMO_API_BASE)?,
            token_mint: Pubkey::from_str(USDC_MINT)?,
        })
    }

    /// Resolves a profile without signing or submitting the prepared transaction.
    pub async fn resolve(&self, input: &str) -> Result<ResolveResult> {
        if let Some(wallet) = parse_solana_wallet(input) {
            return resolve_solana_with_client(&self.relay, wallet).await;
        }

        let total_started = Instant::now();
        let handle = normalize_handle(input)?;

        let profile_started = Instant::now();
        let profile = self.fetch_profile(&handle).await?;
        let profile_elapsed = profile_started.elapsed();
        let user_id = first_string(
            &profile,
            &["/id", "/responseObject/id", "/data/id", "/user/id"],
        )
        .ok_or_else(|| anyhow!("FOMO profile did not contain a user id"))?;

        let build_transfer = async {
            let started = Instant::now();
            (self.build_transfer(user_id).await, started.elapsed())
        };
        let (prepared, swaps) = tokio::join!(build_transfer, self.fetch_swaps(user_id));
        let (prepared_response, builder_elapsed) = prepared;
        let prepared_response = prepared_response?;

        let decode_started = Instant::now();
        let prepared =
            decode_prepared_transfer(&prepared_response, TRANSFER_AMOUNT, &self.token_mint)?;
        let decode_elapsed = decode_started.elapsed();

        let rpc_started = Instant::now();
        let account_keys = self.resolve_account_keys(&prepared.transaction).await?;
        let transfer = transfer_accounts(
            &prepared.transaction,
            &account_keys,
            &self.token_mint,
            prepared.recipient_amount_hint,
        )?;

        let wallet = if let Some(owner) = ata_owner_for_destination(
            &prepared.transaction,
            &account_keys,
            &transfer.destination,
            &self.token_mint,
        )? {
            owner
        } else {
            let destination = self
                .rpc_get_account(&transfer.destination)
                .await?
                .ok_or_else(|| anyhow!("destination token account does not exist"))?;
            token_account_owner(&destination, &self.token_mint)?
        };
        let rpc_elapsed = rpc_started.elapsed();

        let evm_started = Instant::now();
        let evm = match swaps {
            Ok(swaps) => match evm::resolve(&self.relay, &wallet.to_string(), &swaps).await {
                Ok(resolution) => resolution,
                Err(_) => EvmResolution::Unavailable(EvmUnavailableReason::RelayUnavailable),
            },
            Err(error) if is_authentication_error(&error) => return Err(error),
            Err(_) => EvmResolution::Unavailable(EvmUnavailableReason::FomoSwapsUnavailable),
        };
        let evm_elapsed = evm_started.elapsed();

        Ok(ResolveResult {
            solana_wallet: wallet,
            evm,
            timings: ResolveTimings {
                profile: profile_elapsed,
                builder: builder_elapsed,
                decode: decode_elapsed,
                rpc: rpc_elapsed,
                evm: evm_elapsed,
                total: total_started.elapsed(),
            },
        })
    }

    async fn fetch_profile(&self, handle: &str) -> Result<Value> {
        let mut url = self.api_base.join("/v2/users/userHandle/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid FOMO profile URL"))?
            .pop_if_empty()
            .push(handle);
        self.fomo_json(Method::GET, url, None).await
    }

    async fn build_transfer(&self, user_id: &str) -> Result<Value> {
        let url = self.api_base.join("/transfers/v2/send")?;
        self.fomo_json(
            Method::POST,
            url,
            Some(json!({
                "destinationUserId": user_id,
                "tokenAddress": USDC_MINT,
                "amount": TRANSFER_AMOUNT.to_string(),
            })),
        )
        .await
    }

    async fn fetch_swaps(&self, user_id: &str) -> Result<Value> {
        let mut url = self.api_base.join("/v2/users/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid FOMO swaps URL"))?
            .pop_if_empty()
            .push(user_id)
            .push("swaps");
        self.fomo_json(Method::GET, url, None).await
    }

    async fn fomo_json(&self, method: Method, url: Url, body: Option<Value>) -> Result<Value> {
        for attempt in 0..MAX_FOMO_ATTEMPTS {
            let mut request = self
                .fomo
                .request(method.clone(), url.as_str())
                .bearer_auth(&self.config.bearer)
                .header("accept", "*/*")
                .header("content-type", "application/json")
                .header("origin", "https://fomo.family")
                .header("referer", "https://fomo.family/")
                .header(
                    "sec-ch-ua",
                    "\"Chromium\";v=\"148\", \"Google Chrome\";v=\"148\", \"Not_A Brand\";v=\"99\"",
                )
                .header("sec-ch-ua-mobile", "?0")
                .header("sec-ch-ua-platform", "\"macOS\"")
                .header("sec-fetch-dest", "empty")
                .header("sec-fetch-mode", "cors")
                .header("sec-fetch-site", "same-site")
                .header("x-supported-chains", SUPPORTED_CHAINS)
                .header(
                    "user-agent",
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36",
                );
            if let Some(value) = &body {
                request = request.json(value);
            }

            let response = request.send().await.context("FOMO request failed")?;
            if is_retryable_fomo_status(response.status()) && attempt + 1 < MAX_FOMO_ATTEMPTS {
                sleep(fomo_retry_delay(response.status(), attempt)).await;
                continue;
            }

            let status = response.status();
            let text = response
                .text()
                .await
                .context("could not read FOMO response")?;
            if !status.is_success() {
                bail!(
                    "FOMO API {} {} returned {status}: {}",
                    method,
                    url.path(),
                    bounded_error_body(&text)
                );
            }
            return serde_json::from_str(&text).context("FOMO returned invalid JSON");
        }
        unreachable!("the retry loop always returns or errors")
    }

    async fn resolve_account_keys(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Vec<Pubkey>> {
        let mut loaded = LoadedAddresses::default();

        if let VersionedMessage::V0(message) = &transaction.message {
            for lookup in &message.address_table_lookups {
                let account = self
                    .rpc_get_account(&lookup.account_key)
                    .await?
                    .ok_or_else(|| {
                        anyhow!("address lookup table {} is missing", lookup.account_key)
                    })?;
                if account.owner_program != address_lookup_table_program::id() {
                    bail!(
                        "address lookup table {} has unexpected owner {}",
                        lookup.account_key,
                        account.owner_program
                    );
                }
                let table = AddressLookupTable::deserialize(&account.data)
                    .map_err(|error| anyhow!("invalid address lookup table: {error:?}"))?;

                append_lookup_addresses(
                    &mut loaded.writable,
                    &table.addresses,
                    &lookup.writable_indexes,
                    "writable",
                )?;
                append_lookup_addresses(
                    &mut loaded.readonly,
                    &table.addresses,
                    &lookup.readonly_indexes,
                    "readonly",
                )?;
            }
        }

        let dynamic = (!loaded.is_empty()).then_some(&loaded);
        Ok(
            AccountKeys::new(transaction.message.static_account_keys(), dynamic)
                .iter()
                .copied()
                .collect(),
        )
    }

    async fn rpc_get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>> {
        let response = self
            .rpc
            .post(&self.config.rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getAccountInfo",
                "params": [
                    address.to_string(),
                    {"encoding": "base64", "commitment": "confirmed"}
                ]
            }))
            .send()
            .await
            .context("Solana RPC request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .await
            .context("Solana RPC returned invalid JSON")?;
        if !status.is_success() {
            bail!("Solana RPC returned HTTP {status}");
        }
        if let Some(error) = value.get("error") {
            bail!("Solana RPC error: {error}");
        }

        let account = value
            .pointer("/result/value")
            .ok_or_else(|| anyhow!("Solana RPC response did not contain result.value"))?;
        if account.is_null() {
            return Ok(None);
        }

        let owner_program = account
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("RPC account did not contain owner"))?
            .parse()
            .context("RPC account owner was not a public key")?;
        let data_b64 = account
            .pointer("/data/0")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("RPC account did not contain base64 data"))?;
        let data = BASE64
            .decode(data_b64)
            .context("RPC account contained invalid base64")?;

        Ok(Some(RpcAccount {
            owner_program,
            data,
        }))
    }
}

pub fn parse_solana_wallet(input: &str) -> Option<Pubkey> {
    Pubkey::from_str(input.trim()).ok()
}

pub async fn resolve_solana_wallet(input: &str) -> Result<ResolveResult> {
    let wallet = parse_solana_wallet(input)
        .ok_or_else(|| anyhow!("input is not a valid Solana wallet address"))?;
    let relay = shared_http_client()?;
    resolve_solana_with_client(&relay, wallet).await
}

async fn resolve_solana_with_client(client: &RpcClient, wallet: Pubkey) -> Result<ResolveResult> {
    let total_started = Instant::now();
    let evm_started = Instant::now();
    let evm = match evm::resolve_wallet(client, &wallet.to_string()).await {
        Ok(resolution) => resolution,
        Err(_) => EvmResolution::Unavailable(EvmUnavailableReason::RelayUnavailable),
    };

    Ok(ResolveResult {
        solana_wallet: wallet,
        evm,
        timings: ResolveTimings {
            profile: Duration::ZERO,
            builder: Duration::ZERO,
            decode: Duration::ZERO,
            rpc: Duration::ZERO,
            evm: evm_started.elapsed(),
            total: total_started.elapsed(),
        },
    })
}

#[derive(Debug)]
pub struct ResolveResult {
    pub solana_wallet: Pubkey,
    pub evm: EvmResolution,
    pub timings: ResolveTimings,
}

#[derive(Clone, Copy, Debug)]
pub struct ResolveTimings {
    pub profile: Duration,
    pub builder: Duration,
    pub decode: Duration,
    pub rpc: Duration,
    pub evm: Duration,
    pub total: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TransferAccounts {
    destination: Pubkey,
    amount: u64,
}

#[derive(Debug)]
struct RpcAccount {
    owner_program: Pubkey,
    data: Vec<u8>,
}

struct PreparedTransfer {
    transaction: VersionedTransaction,
    recipient_amount_hint: Option<u64>,
}

fn decode_prepared_transfer(
    prepared: &Value,
    requested_amount: u64,
    expected_mint: &Pubkey,
) -> Result<PreparedTransfer> {
    let transaction_b64 = first_string(
        prepared,
        &[
            "/responseObject/transferTransaction/transferTransaction",
            "/transferTransaction/transferTransaction",
            "/responseObject/transferTransaction",
            "/transferTransaction",
        ],
    )
    .ok_or_else(|| anyhow!("FOMO send response did not contain a prepared transaction"))?;
    let bytes = BASE64
        .decode(transaction_b64)
        .context("prepared transaction was not valid base64")?;
    let transaction =
        bincode::deserialize(&bytes).context("could not decode the Solana transaction")?;

    let fee_token = first_string(
        prepared,
        &[
            "/responseObject/transferFeeTokenAddress",
            "/transferFeeTokenAddress",
        ],
    );
    let fee_usd = first_value(
        prepared,
        &["/responseObject/transferFeeUsd", "/transferFeeUsd"],
    );
    let recipient_amount_hint =
        if expected_mint.to_string() == USDC_MINT && fee_token == Some(USDC_MINT) {
            fee_usd
                .and_then(usdc_raw_from_usd)
                .and_then(|fee| requested_amount.checked_sub(fee))
        } else {
            None
        };

    Ok(PreparedTransfer {
        transaction,
        recipient_amount_hint,
    })
}

fn transfer_accounts(
    transaction: &VersionedTransaction,
    keys: &[Pubkey],
    expected_mint: &Pubkey,
    recipient_amount_hint: Option<u64>,
) -> Result<TransferAccounts> {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;
    let token_2022_program = Pubkey::from_str(TOKEN_2022_PROGRAM)?;
    let mut checked = Vec::new();
    let mut unchecked = Vec::new();

    for instruction in transaction.message.instructions() {
        let program = key(keys, instruction.program_id_index)?;
        if program != token_program && program != token_2022_program {
            continue;
        }

        let is_checked = instruction.data.first() == Some(&12)
            || (program == token_2022_program && instruction.data.starts_with(&[26, 1]));
        if is_checked {
            if instruction.accounts.len() < 4 {
                bail!("checked SPL transfer instruction has too few accounts");
            }
            let amount = checked_transfer_amount(&instruction.data)?;
            let mint = key(keys, instruction.accounts[1])?;
            if mint == *expected_mint {
                checked.push(TransferAccounts {
                    destination: key(keys, instruction.accounts[2])?,
                    amount,
                });
            }
        } else if instruction.data.first() == Some(&3) {
            if instruction.accounts.len() < 3 {
                bail!("unchecked SPL transfer instruction has too few accounts");
            }
            unchecked.push(TransferAccounts {
                destination: key(keys, instruction.accounts[1])?,
                amount: transfer_amount(&instruction.data)?,
            });
        }
    }

    if checked.is_empty() {
        unique_transfer(unchecked, "unchecked", recipient_amount_hint)
    } else {
        unique_transfer(checked, "checked", recipient_amount_hint)
    }
}

fn checked_transfer_amount(data: &[u8]) -> Result<u64> {
    let range = match data {
        [12, ..] if data.len() == 10 => 1..9,
        [26, 1, ..] if data.len() == 19 => 2..10,
        _ => bail!("checked SPL transfer data has an invalid layout"),
    };
    let bytes = data
        .get(range)
        .ok_or_else(|| anyhow!("checked SPL transfer data is too short"))?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn transfer_amount(data: &[u8]) -> Result<u64> {
    if data.len() != 9 || data.first() != Some(&3) {
        bail!("SPL transfer data has an invalid layout");
    }
    let bytes = data
        .get(1..9)
        .ok_or_else(|| anyhow!("SPL transfer data is too short"))?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn unique_transfer(
    mut matches: Vec<TransferAccounts>,
    kind: &str,
    amount_hint: Option<u64>,
) -> Result<TransferAccounts> {
    if let Some(amount) = amount_hint {
        matches.retain(|transfer| transfer.amount == amount);
    }

    match matches.as_slice() {
        [only] => Ok(*only),
        [] => bail!("no {kind} SPL transfer destination found"),
        _ => bail!("prepared transaction contains multiple {kind} SPL transfer destinations"),
    }
}

fn ata_owner_for_destination(
    transaction: &VersionedTransaction,
    keys: &[Pubkey],
    destination: &Pubkey,
    expected_mint: &Pubkey,
) -> Result<Option<Pubkey>> {
    let ata_program = Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM)?;
    let mut owners = Vec::new();

    for instruction in transaction.message.instructions() {
        if key(keys, instruction.program_id_index)? != ata_program || instruction.accounts.len() < 6
        {
            continue;
        }
        let ata = key(keys, instruction.accounts[1])?;
        let owner = key(keys, instruction.accounts[2])?;
        let mint = key(keys, instruction.accounts[3])?;
        let token_program = key(keys, instruction.accounts[5])?;
        validate_token_program(&token_program)?;
        let expected_ata = Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ata_program,
        )
        .0;
        if ata == *destination && ata == expected_ata && mint == *expected_mint {
            owners.push(owner);
        }
    }

    match owners.as_slice() {
        [] => Ok(None),
        [owner] => Ok(Some(*owner)),
        _ => bail!("prepared transaction contains multiple matching ATA owners"),
    }
}

fn token_account_owner(account: &RpcAccount, expected_mint: &Pubkey) -> Result<Pubkey> {
    validate_token_program(&account.owner_program)?;
    let mint_bytes = account
        .data
        .get(0..32)
        .ok_or_else(|| anyhow!("destination token account data is too short"))?;
    let owner_bytes = account
        .data
        .get(32..64)
        .ok_or_else(|| anyhow!("destination token account data is too short"))?;
    let mint = Pubkey::new_from_array(mint_bytes.try_into().unwrap());
    if mint != *expected_mint {
        bail!("prepared transaction destination uses an unexpected mint");
    }
    Ok(Pubkey::new_from_array(owner_bytes.try_into().unwrap()))
}

fn validate_token_program(owner: &Pubkey) -> Result<()> {
    let legacy = Pubkey::from_str(TOKEN_PROGRAM)?;
    let token_2022 = Pubkey::from_str(TOKEN_2022_PROGRAM)?;
    if *owner != legacy && *owner != token_2022 {
        bail!("account is not owned by an SPL token program");
    }
    Ok(())
}

fn append_lookup_addresses(
    output: &mut Vec<Pubkey>,
    table: &[Pubkey],
    indexes: &[u8],
    kind: &str,
) -> Result<()> {
    for index in indexes {
        output.push(
            *table
                .get(*index as usize)
                .ok_or_else(|| anyhow!("lookup-table {kind} index is out of range"))?,
        );
    }
    Ok(())
}

fn key(keys: &[Pubkey], index: u8) -> Result<Pubkey> {
    keys.get(index as usize)
        .copied()
        .ok_or_else(|| anyhow!("transaction account index {index} is out of range"))
}

fn first_string<'a>(value: &'a Value, pointers: &[&str]) -> Option<&'a str> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
}

fn first_value<'a>(value: &'a Value, pointers: &[&str]) -> Option<&'a Value> {
    pointers.iter().find_map(|pointer| value.pointer(pointer))
}

fn usdc_raw_from_usd(value: &Value) -> Option<u64> {
    let dollars = value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())?;
    if !dollars.is_finite() || dollars < 0.0 {
        return None;
    }
    Some((dollars * 1_000_000.0).round() as u64)
}

fn normalize_handle(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("FOMO handle cannot be empty");
    }

    let handle = if let Ok(url) = Url::parse(trimmed) {
        match url.host_str() {
            Some("fomo.family" | "www.fomo.family") => {}
            _ => bail!("profile URL must use fomo.family"),
        }
        url.path_segments()
            .and_then(|mut segments| segments.rfind(|part| !part.is_empty()))
            .ok_or_else(|| anyhow!("profile URL does not contain a handle"))?
            .to_owned()
    } else {
        trimmed.trim_start_matches('@').to_owned()
    };

    if handle.is_empty() || handle.contains('/') {
        bail!("invalid FOMO handle");
    }
    Ok(handle)
}

fn is_retryable_fomo_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn fomo_retry_delay(status: StatusCode, attempt: u32) -> Duration {
    let base_ms = if status == StatusCode::TOO_MANY_REQUESTS {
        1_000
    } else {
        250
    };
    Duration::from_millis(base_ms * (1u64 << attempt.min(3)))
}

fn bounded_error_body(body: &str) -> String {
    let mut bounded = body.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if body.chars().count() > MAX_ERROR_BODY_CHARS {
        bounded.push_str("...");
    }
    bounded
}

fn is_authentication_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("401 Unauthorized") || message.contains("JWT authentication middleware")
}

#[cfg(test)]
mod tests {
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_message::{Message, v0};
    use solana_transaction::Transaction;

    use super::*;

    fn checked_transfer(mint: Pubkey, destination: Pubkey, amount: u64) -> Instruction {
        let mut data = vec![12];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(6);
        Instruction {
            program_id: Pubkey::from_str(TOKEN_PROGRAM).unwrap(),
            accounts: vec![
                AccountMeta::new(Pubkey::new_unique(), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(Pubkey::new_unique(), true),
            ],
            data,
        }
    }

    fn prepared_response(transaction: &VersionedTransaction) -> Value {
        json!({
            "transferTransaction": BASE64.encode(bincode::serialize(transaction).unwrap()),
        })
    }

    #[test]
    fn normalizes_handle_forms() {
        assert_eq!(normalize_handle("ExampleHandle").unwrap(), "ExampleHandle");
        assert_eq!(normalize_handle("@ExampleHandle").unwrap(), "ExampleHandle");
        assert_eq!(
            normalize_handle("https://fomo.family/profile/ExampleHandle/").unwrap(),
            "ExampleHandle"
        );
    }

    #[test]
    fn rejects_non_fomo_profile_url() {
        assert!(normalize_handle("https://example.com/profile/test").is_err());
    }

    #[test]
    fn recognizes_solana_wallet_inputs() {
        let wallet = Pubkey::new_unique();
        assert_eq!(parse_solana_wallet(&wallet.to_string()), Some(wallet));
        assert_eq!(parse_solana_wallet("not-a-wallet"), None);
    }

    #[test]
    fn decodes_legacy_token_account_owner() {
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut data = vec![0; 165];
        data[0..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(owner.as_ref());
        let account = RpcAccount {
            owner_program: Pubkey::from_str(TOKEN_PROGRAM).unwrap(),
            data,
        };

        assert_eq!(token_account_owner(&account, &mint).unwrap(), owner);
    }

    #[test]
    fn rejects_unexpected_token_mint() {
        let mint = Pubkey::new_unique();
        let mut data = vec![0; 165];
        data[0..32].copy_from_slice(Pubkey::new_unique().as_ref());
        let account = RpcAccount {
            owner_program: Pubkey::from_str(TOKEN_PROGRAM).unwrap(),
            data,
        };

        assert!(token_account_owner(&account, &mint).is_err());
    }

    #[test]
    fn decodes_legacy_transfer_and_ata_owner() {
        let payer = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let token_program = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
        let ata_program = Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM).unwrap();
        let destination = Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ata_program,
        )
        .0;
        let ata = Instruction {
            program_id: ata_program,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(owner, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new_readonly(Pubkey::new_unique(), false),
                AccountMeta::new_readonly(token_program, false),
            ],
            data: vec![1],
        };
        let transfer = checked_transfer(mint, destination, 1_900_000);
        let transaction = VersionedTransaction::from(Transaction::new_unsigned(Message::new(
            &[ata, transfer],
            Some(&payer),
        )));

        let decoded =
            decode_prepared_transfer(&prepared_response(&transaction), 2_000_000, &mint).unwrap();
        let keys = decoded.transaction.message.static_account_keys().to_vec();
        let selected = transfer_accounts(&decoded.transaction, &keys, &mint, None).unwrap();

        assert_eq!(selected.destination, destination);
        assert_eq!(selected.amount, 1_900_000);
        assert_eq!(
            ata_owner_for_destination(&decoded.transaction, &keys, &destination, &mint).unwrap(),
            Some(owner)
        );
    }

    #[test]
    fn walks_v0_checked_transfer() {
        let payer = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let instruction = checked_transfer(mint, destination, 1_800_000);
        let message =
            v0::Message::try_compile(&payer, &[instruction], &[], Hash::new_unique()).unwrap();
        let transaction = VersionedTransaction {
            signatures: Vec::new(),
            message: VersionedMessage::V0(message),
        };
        let keys = transaction.message.static_account_keys().to_vec();

        let selected = transfer_accounts(&transaction, &keys, &mint, None).unwrap();

        assert_eq!(selected.destination, destination);
        assert_eq!(selected.amount, 1_800_000);
    }

    #[test]
    fn selects_recipient_transfer_from_fee_pair() {
        let recipient = TransferAccounts {
            destination: Pubkey::new_unique(),
            amount: 1_759_481,
        };
        let fee = TransferAccounts {
            destination: Pubkey::new_unique(),
            amount: 240_519,
        };

        assert_eq!(
            unique_transfer(vec![recipient, fee], "checked", Some(recipient.amount)).unwrap(),
            recipient
        );
    }

    #[test]
    fn rejects_malformed_checked_transfers() {
        let payer = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();

        let mut missing_decimals = checked_transfer(mint, destination, 1_900_000);
        missing_decimals.data.pop();
        let transaction = VersionedTransaction::from(Transaction::new_unsigned(Message::new(
            &[missing_decimals],
            Some(&payer),
        )));
        let keys = transaction.message.static_account_keys().to_vec();
        assert!(transfer_accounts(&transaction, &keys, &mint, None).is_err());

        let mut missing_authority = checked_transfer(mint, destination, 1_900_000);
        missing_authority.accounts.pop();
        let transaction = VersionedTransaction::from(Transaction::new_unsigned(Message::new(
            &[missing_authority],
            Some(&payer),
        )));
        let keys = transaction.message.static_account_keys().to_vec();
        assert!(transfer_accounts(&transaction, &keys, &mint, None).is_err());
    }

    #[test]
    fn rejects_single_transfer_that_misses_amount_hint() {
        let recipient = TransferAccounts {
            destination: Pubkey::new_unique(),
            amount: 1_759_481,
        };

        assert!(unique_transfer(vec![recipient], "checked", Some(1_759_480)).is_err());
    }

    #[test]
    fn converts_usdc_fee_to_raw_units() {
        assert_eq!(usdc_raw_from_usd(&json!(0.240519)), Some(240_519));
    }

    #[test]
    fn retries_rate_limits_and_server_errors_only() {
        assert!(is_retryable_fomo_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_fomo_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable_fomo_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_fomo_status(StatusCode::UNAUTHORIZED));
    }
}
