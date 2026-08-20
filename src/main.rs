mod browser_auth;
mod ui;

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

use anyhow::{Context, Result, anyhow, bail};
use browser_auth::{copy_browser_exporter, format_duration, prompt_browser_auth, unix_timestamp};
use fomo_wallet_resolver::{
    EvmResolution, FomoWalletResolver, ResolveResult, ResolverConfig, parse_solana_wallet,
    resolve_solana_wallet,
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse()?;
    if cli.help {
        print_help();
        return Ok(());
    }

    if cli.ui || (!cli.session && cli.input.is_none()) {
        if cli.session || cli.input.is_some() {
            bail!("--ui cannot be combined with --session or a handle argument");
        }
        return ui::run().await;
    }

    if cli.session {
        if cli.input.is_some() {
            bail!("--session does not accept a handle argument");
        }
        return run_session(&cli).await;
    }

    let input = cli
        .input
        .as_deref()
        .ok_or_else(|| anyhow!("missing wallet, handle, or profile URL; use --help for usage"))?;
    let result = if parse_solana_wallet(input).is_some() {
        resolve_solana_wallet(input).await?
    } else {
        let bearer = env::var("FOMO_BEARER_TOKEN")
            .context("set FOMO_BEARER_TOKEN to resolve a FOMO handle or profile")?;
        let config = ResolverConfig::from_env(bearer)?;
        FomoWalletResolver::new(config)?.resolve(input).await?
    };

    print_result(None, &result, &cli);
    Ok(())
}

async fn run_session(cli: &Cli) -> Result<()> {
    let (mut resolver, mut expires_at) = authenticated_session()?;
    eprintln!("Session ready. Enter a wallet, handle, or profile URL. Type quit to exit.");
    let stdin = io::stdin();
    loop {
        eprint!("> ");
        io::stderr().flush()?;

        let mut input = String::new();
        if stdin.read_line(&mut input)? == 0 {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input.to_ascii_lowercase().as_str(), "quit" | "exit") {
            break;
        }

        if expires_at <= unix_timestamp()?.saturating_add(30) {
            eprintln!("JWT expired or is about to expire. Refreshing authentication.");
            (resolver, expires_at) = authenticated_session()?;
        }

        match resolver.resolve(input).await {
            Ok(result) => print_result(Some(input), &result, cli),
            Err(error) if is_authentication_error(&error) => {
                eprintln!("FOMO rejected the JWT. Refreshing authentication, then retrying.");
                (resolver, expires_at) = authenticated_session()?;
                match resolver.resolve(input).await {
                    Ok(result) => print_result(Some(input), &result, cli),
                    Err(retry_error) => eprintln!("{input}: error after refresh: {retry_error:#}"),
                }
            }
            Err(error) => eprintln!("{input}: error: {error:#}"),
        }
    }
    Ok(())
}

fn authenticated_session() -> Result<(FomoWalletResolver, u64)> {
    copy_browser_exporter()?;
    eprintln!("Browser helper copied.");
    eprintln!("Paste it into the FOMO browser console and press Enter.");
    let auth = prompt_browser_auth()?;
    let remaining = auth.remaining_seconds()?;
    eprintln!("JWT expires in {}.", format_duration(remaining));

    let config = ResolverConfig::from_env(auth.bearer)?;
    let resolver = FomoWalletResolver::new(config)?;
    Ok((resolver, auth.expires_at))
}

fn is_authentication_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("401 Unauthorized") || message.contains("JWT authentication middleware")
}

fn print_result(input: Option<&str>, result: &ResolveResult, cli: &Cli) {
    if let Some(input) = input {
        println!("{input}");
    }
    println!("Solana: {}", result.solana_wallet);
    match &result.evm {
        EvmResolution::Resolved(wallet) => println!("EVM: {wallet}"),
        EvmResolution::Unavailable(reason) => println!("EVM: unavailable ({})", reason.message()),
    }
    if cli.timings {
        eprintln!("profile_ms={}", result.timings.profile.as_millis());
        eprintln!("builder_ms={}", result.timings.builder.as_millis());
        eprintln!("decode_ms={}", result.timings.decode.as_millis());
        eprintln!("rpc_ms={}", result.timings.rpc.as_millis());
        eprintln!("evm_ms={}", result.timings.evm.as_millis());
        eprintln!("total_ms={}", result.timings.total.as_millis());
    }
}

#[derive(Default)]
struct Cli {
    input: Option<String>,
    timings: bool,
    session: bool,
    ui: bool,
    help: bool,
}

impl Cli {
    fn parse() -> Result<Self> {
        let mut cli = Self::default();
        for argument in env::args().skip(1) {
            match argument.as_str() {
                "-h" | "--help" => cli.help = true,
                "--timings" => cli.timings = true,
                "--session" => cli.session = true,
                "--ui" => cli.ui = true,
                value if value.starts_with('-') => bail!("unknown option: {value}"),
                value if cli.input.is_none() => cli.input = Some(value.to_owned()),
                value => bail!("unexpected extra argument: {value}"),
            }
        }
        Ok(cli)
    }
}

fn print_help() {
    println!(
        "fomo-wallet-resolver [--ui]\n\
         fomo-wallet-resolver [OPTIONS] <WALLET_HANDLE_OR_PROFILE_URL>\n\
         fomo-wallet-resolver [OPTIONS] --session\n\n\
         Options:\n  \
           --ui              Open the local browser interface (default)\n  \
           --session         Paste browser auth once, then resolve repeatedly\n  \
           --timings         Print request and RPC timings\n  \
         -h, --help          Print this help"
    );
}
