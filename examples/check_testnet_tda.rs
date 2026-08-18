//! Find and inspect a testnet validator's Tip Distribution Account (TDA).
//!
//! Examples:
//!     cargo run --example check_testnet_tda
//!     cargo run --example check_testnet_tda -- --previous-epoch
//!     cargo run --example check_testnet_tda -- --epoch 1002
//!     SOLANA_RPC_URL=https://my-rpc.example cargo run --example check_testnet_tda
//!     cargo run --example check_testnet_tda -- --sender ~/.config/solana/id.json
//!
//! Exit codes: 0 = TDA looks healthy, 1 = TDA not created (or the run failed),
//! 2 = TDA has an unexpected owner.

use std::{
    error::Error,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::{read_keypair_file, Keypair};
use solana_message::v0;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::{versioned::VersionedTransaction, Signature, VersionedMessage};

// Testnet addresses
const TESTNET_TIP_DISTRIBUTION_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("DzvGET57TAgEDxvm3ERUM4GNcsAJdqjDLCne9sdfY4wf");
const TESTNET_VOTE_ACCOUNT: Pubkey =
    Pubkey::from_str_const("Eg1ZgUARDGx3ewjtyLdvQRoHzpKzSsntYwATWpnkjgTz");

const TIP_DISTRIBUTION_SEED: &[u8] = b"TIP_DISTRIBUTION_ACCOUNT";
const DEFAULT_RPC_URL: &str = "https://api.testnet.solana.com";
const DEFAULT_MIN_CLAIM_LAMPORTS: u64 = 5_000;
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
const TOP_UP_LAMPORTS: u64 = 1_000_000; // 0.001 SOL

#[derive(Parser)]
#[command(about = "Find and inspect a testnet validator's Tip Distribution Account (TDA).")]
struct Args {
    /// Epoch to inspect (defaults to the current testnet epoch)
    #[arg(long, conflicts_with = "previous_epoch")]
    epoch: Option<u64>,

    /// Inspect the epoch immediately before the current testnet epoch
    #[arg(long)]
    previous_epoch: bool,

    /// Solana JSON-RPC URL (default: SOLANA_RPC_URL or public testnet RPC)
    #[arg(long, env = "SOLANA_RPC_URL", default_value = DEFAULT_RPC_URL)]
    rpc_url: String,

    /// Claim threshold
    #[arg(long, default_value_t = DEFAULT_MIN_CLAIM_LAMPORTS)]
    min_claim_lamports: u64,

    /// Path to a Solana CLI JSON keypair. If the checked TDA is below the claim
    /// threshold, send it 0.001 SOL.
    #[arg(long)]
    sender: Option<PathBuf>,
}

fn derive_tda(vote_account: &Pubkey, epoch: u64) -> Pubkey {
    let (tda, _bump) = Pubkey::find_program_address(
        &[
            TIP_DISTRIBUTION_SEED,
            vote_account.as_ref(),
            &epoch.to_le_bytes(),
        ],
        &TESTNET_TIP_DISTRIBUTION_PROGRAM_ID,
    );
    tda
}

fn sol(lamports: u64) -> String {
    format!(
        "{}.{:09} SOL",
        lamports / LAMPORTS_PER_SOL,
        lamports % LAMPORTS_PER_SOL
    )
}

fn load_keypair(keypair_path: &Path) -> Result<Keypair, Box<dyn Error>> {
    read_keypair_file(keypair_path).map_err(|error| {
        format!(
            "Could not read Solana keypair from {}: {error}",
            keypair_path.display()
        )
        .into()
    })
}

async fn send_top_up(
    client: &RpcClient,
    sender_path: &Path,
    recipient: &Pubkey,
) -> Result<Signature, Box<dyn Error>> {
    let sender = load_keypair(sender_path)?;
    let instruction = transfer(&sender.pubkey(), recipient, TOP_UP_LAMPORTS);

    let latest_blockhash = client.get_latest_blockhash().await?;
    let message =
        v0::Message::try_compile(&sender.pubkey(), &[instruction], &[], latest_blockhash)?;
    let transaction = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&sender])?;
    Ok(client.send_transaction(&transaction).await?)
}

async fn run() -> Result<ExitCode, Box<dyn Error>> {
    let args = Args::parse();
    let vote_account = TESTNET_VOTE_ACCOUNT;
    let client =
        RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::confirmed());

    let epoch = match args.epoch {
        Some(epoch) => epoch,
        None => {
            let current_epoch = client.get_epoch_info().await?.epoch;
            if args.previous_epoch {
                current_epoch.saturating_sub(1)
            } else {
                current_epoch
            }
        }
    };

    let tda = derive_tda(&vote_account, epoch);
    let account = client
        .get_account_with_commitment(&tda, CommitmentConfig::confirmed())
        .await?
        .value;

    println!("Epoch: {epoch}");
    println!("Vote account: {vote_account}");
    println!("TDA: {tda}");

    let Some(account) = account else {
        println!("Status: not created");
        return Ok(ExitCode::from(1));
    };

    if account.owner != TESTNET_TIP_DISTRIBUTION_PROGRAM_ID {
        eprintln!("Status: unexpected owner ({})", account.owner);
        return Ok(ExitCode::from(2));
    }

    let space = account.data.len();
    let balance = account.lamports;
    let rent_exempt_minimum = client.get_minimum_balance_for_rent_exemption(space).await?;
    let claimable = balance.saturating_sub(rent_exempt_minimum);

    println!("Status: created");
    println!("Account size: {space} bytes");
    println!("Total balance: {balance} lamports ({})", sol(balance));
    println!(
        "Rent-exempt reserve: {rent_exempt_minimum} lamports ({})",
        sol(rent_exempt_minimum)
    );
    println!("Claimable: {claimable} lamports ({})", sol(claimable));
    println!(
        "Meets {}-lamport claim threshold: {}",
        args.min_claim_lamports,
        if claimable >= args.min_claim_lamports {
            "yes"
        } else {
            "no"
        }
    );

    if let Some(sender_path) = &args.sender {
        if claimable < args.min_claim_lamports {
            let signature = send_top_up(&client, sender_path, &tda).await?;
            println!(
                "Sent {} from {}",
                sol(TOP_UP_LAMPORTS),
                sender_path.display()
            );
            println!("Transaction: {signature}");
        }
    }

    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
