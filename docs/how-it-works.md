# How wallet resolution works

The resolver supports two inputs:

- A FOMO username or profile URL, which resolves both the Solana wallet and any
  verifiable EVM wallet.
- A Solana wallet, which resolves an EVM wallet without FOMO authentication or
  Solana RPC.

## FOMO profile to Solana wallet

```text
FOMO handle or profile URL
-> profile API returns the user ID
-> transfer API builds an unsigned USDC transfer
-> resolver decodes the Solana transaction
-> destination token account is identified
-> wallet owner is read from the ATA instruction or Solana RPC
```

FOMO's transfer builder prepares a transaction for the target user. That
transaction contains the destination SPL token account. If it also creates the
destination associated token account, the creation instruction contains the
wallet owner. If the token account already exists, the resolver reads its owner
through Solana RPC.

The default prepared amount is 2 USDC because the builder subtracts its dynamic
fee from that amount. The connected account does not need a USDC balance because
the prepared transaction is decoded only and never submitted.

## Solana wallet to EVM wallet

```text
resolved Solana wallet + FOMO swap history
-> successful Solana-to-EVM FOMO swaps are selected
-> Relay requests for that Solana wallet are read
-> timestamp, amount, fee, chains, tokens, and recipient credit must match
-> latest uniquely verified EVM recipient is returned
```

EVM resolution is deliberately conditional. The resolver requires a completed
FOMO Relay swap whose FOMO record matches a successful Relay request and whose
destination state changes credit the same recipient.

If evidence is missing, conflicting, or temporarily unavailable, the Solana
wallet is still returned and the EVM wallet is shown as unavailable. The
resolver does not guess from profile fields or unrelated activity.

## Direct Solana input

```text
Solana wallet input
-> successful FOMO Relay requests for that sender are read directly
-> sender, recipient, chains, tokens, and destination credit are verified
-> one consistent EVM recipient is returned
```

A direct Solana-wallet lookup does not require FOMO authentication or a Solana
RPC. It uses successful Relay requests attributed to FOMO and requires the
request metadata and destination state changes to agree on the EVM recipient.
Conflicting recipients are reported as unavailable instead of selecting one.

EVM evidence comes only from FOMO swap history and Relay's public request API.
The resolver does not use a third-party wallet-resolution database.

## Authentication and boundaries

The browser console helper copies authentication from an existing signed-in
FOMO tab. The resolver keeps accepted credentials in process memory and
conditionally clears the accepted auth code from the clipboard. It does not
write credentials to disk or print them.

The live JWT timer updates every second. Automatic refresh begins five minutes
before expiry and retries every 30 seconds until Privy returns a newer token.
Operating-system clipboard history or clipboard-sync tools may still retain
copied values, so the flow should not be used on shared machines.

The local interface binds only to `127.0.0.1` and sends no-store responses. No
private key or signature is requested, and no prepared transaction is signed,
broadcast, or submitted.

FOMO and Privy use internal APIs in this flow. Upstream API or transaction
format changes may require updates to the browser helper or decoder.
