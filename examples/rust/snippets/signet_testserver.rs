//! Drive a node on a self-hosted `gltestserver` attached to a custom signet.
//!
//! The testserver writes a `.env` with the scheduler URI and the
//! certificates; `gl-client` picks those up from the environment:
//!
//! ```bash
//! source ~/.gltestserver/.env
//! cargo run --bin signet_testserver
//! ```
//!
//! State (seed + credentials) lives in `$GL_EXAMPLE_DIR`, defaulting to
//! `~/.gl-signet-rust`. The testserver keeps its nodes in memory, so delete
//! that directory after restarting the server to register again.
//!
//! Set `GL_LSP_ID` (and optionally `GL_LSP_ADDR`, default
//! `10.113.157.184:39735`) to also request an LSPS2 (JIT channel) invoice
//! from a CLN node running `experimental-lsps2-service`. The example then
//! keeps the signer attached and waits until that invoice is paid, which is
//! when the LSP opens the JIT channel.
use anyhow::{Context, Result};
use gl_client::{
    bitcoin::Network,
    credentials::{Device, Nobody},
    node::{Client, ClnClient},
    pb::{
        cln::{self, amount_or_any, Amount, AmountOrAny},
        LspInvoiceRequest,
    },
    scheduler::Scheduler,
    signer::Signer,
};
use std::{env, fs, path::PathBuf};

const NETWORK: Network = Network::Signet;
const DEFAULT_LSP_ADDR: &str = "10.113.157.184:39735";

fn data_dir() -> PathBuf {
    env::var("GL_EXAMPLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::var("HOME").unwrap()).join(".gl-signet-rust"))
}

fn load_or_create_seed(dir: &PathBuf) -> Result<Vec<u8>> {
    let path = dir.join("hsm_secret");
    if let Ok(seed) = fs::read(&path) {
        return Ok(seed);
    }
    let seed: [u8; 32] = rand::random();
    fs::write(&path, seed)?;
    println!("Generated new seed at {}", path.display());
    Ok(seed.to_vec())
}

/// Register the node once and persist the device credentials.
async fn load_or_register(dir: &PathBuf, seed: &[u8]) -> Result<Device> {
    let path = dir.join("credentials.gfs");
    if !path.exists() {
        // `Nobody` reads GL_NOBODY_CRT, GL_NOBODY_KEY and GL_CA_CRT.
        let nobody = Nobody::new();
        let signer = Signer::new(seed.to_vec(), NETWORK, nobody.clone())?;
        let scheduler = Scheduler::new(NETWORK, nobody).await?;
        let res = scheduler
            .register(&signer, None)
            .await
            .context("registration failed, is the testserver running and .env sourced?")?;
        fs::write(&path, &res.creds)?;
        println!("Registered node, credentials saved to {}", path.display());
    }
    Ok(Device::from_path(&path))
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir = data_dir();
    fs::create_dir_all(&dir)?;

    let seed = load_or_create_seed(&dir)?;
    let creds = load_or_register(&dir, &seed).await?;

    // The signer holds the keys and must be attached for anything that
    // signs (invoices, payments, channel operations).
    let signer = Signer::new(seed, NETWORK, creds.clone())?;
    let (shutdown, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let signer_task = tokio::spawn(async move { signer.run_forever(shutdown_rx).await });

    // Schedules the node if it isn't running yet and connects to it.
    let scheduler = Scheduler::new(NETWORK, creds).await?;
    let mut node: ClnClient = scheduler.node().await?;

    let info = node.getinfo(cln::GetinfoRequest::default()).await?.into_inner();
    println!(
        "Node {} on {} at height {}",
        hex::encode(&info.id),
        info.network,
        info.blockheight
    );

    let addr = node
        .new_addr(cln::NewaddrRequest::default())
        .await?
        .into_inner();
    println!("Fund me at: {}", addr.bech32.unwrap_or_default());

    let funds = node
        .list_funds(cln::ListfundsRequest::default())
        .await?
        .into_inner();
    let onchain: u64 = funds
        .outputs
        .iter()
        .filter_map(|o| o.amount_msat.as_ref().map(|a| a.msat))
        .sum();
    println!("On-chain balance: {} sat in {} outputs", onchain / 1000, funds.outputs.len());

    let invoice = node
        .invoice(cln::InvoiceRequest {
            amount_msat: Some(AmountOrAny {
                value: Some(amount_or_any::Value::Amount(Amount { msat: 10_000 })),
            }),
            description: "signet test".to_string(),
            label: format!("label_{}", rand::random::<u32>()),
            ..Default::default()
        })
        .await?
        .into_inner();
    println!("Invoice: {}", invoice.bolt11);

    // LSPS2: the node asks its *connected* peers for JIT channel offers,
    // so the LSP must be a peer first. Connecting needs the signer for
    // the Noise handshake, which is why it's already running above.
    match env::var("GL_LSP_ID") {
        Ok(lsp_id) => {
            let lsp_addr = env::var("GL_LSP_ADDR").unwrap_or_else(|_| DEFAULT_LSP_ADDR.into());
            let (host, port) = lsp_addr
                .rsplit_once(':')
                .context("GL_LSP_ADDR must be host:port")?;
            node.connect_peer(cln::ConnectRequest {
                id: lsp_id.clone(),
                host: Some(host.to_string()),
                port: Some(port.parse()?),
            })
            .await
            .context("connecting to the LSP")?;
            println!("Connected to LSP {}@{}", lsp_id, lsp_addr);

            // `LspInvoice` lives on the Greenlight service, not on the CLN one.
            let mut gl_node: Client = scheduler.node().await?;
            let label = format!("jit_{}", rand::random::<u32>());
            let jit = gl_node
                .lsp_invoice(LspInvoiceRequest {
                    lsp_id: lsp_id.clone(),
                    token: String::new(),
                    amount_msat: 100_000_000,
                    description: "signet JIT channel".to_string(),
                    label: label.clone(),
                })
                .await
                .context("requesting an LSPS2 invoice")?
                .into_inner();
            println!(
                "JIT invoice (opening fee {} msat): {}",
                jit.opening_fee_msat, jit.bolt11
            );

            // Keep the signer attached while waiting: accepting the LSP's
            // channel and the incoming HTLC both need signatures.
            println!("Waiting for the JIT invoice to be paid (Ctrl-C to stop)...");
            let paid = node
                .wait_invoice(cln::WaitinvoiceRequest { label })
                .await
                .context("waiting for the JIT invoice")?
                .into_inner();
            if paid.status() != cln::waitinvoice_response::WaitinvoiceStatus::Paid {
                anyhow::bail!("JIT invoice {:?} without being paid", paid.status());
            }
            println!(
                "Paid! Received {} msat for an invoice of {} msat",
                paid.amount_received_msat.map(|a| a.msat).unwrap_or(0),
                paid.amount_msat.map(|a| a.msat).unwrap_or(0),
            );

            let channels = node
                .list_peer_channels(cln::ListpeerchannelsRequest {
                    id: Some(hex::decode(&lsp_id)?),
                })
                .await?
                .into_inner()
                .channels;
            for ch in channels {
                println!(
                    "Channel with LSP: state={:?} scid={} alias={} to_us={} msat of {} msat",
                    ch.state(),
                    ch.short_channel_id.as_deref().unwrap_or("-"),
                    ch.alias.as_ref().and_then(|a| a.local.as_deref()).unwrap_or("-"),
                    ch.to_us_msat.map(|a| a.msat).unwrap_or(0),
                    ch.total_msat.map(|a| a.msat).unwrap_or(0),
                );
            }
        }
        Err(_) => println!("GL_LSP_ID not set, skipping the LSPS2 invoice"),
    }

    shutdown.send(()).await.ok();
    signer_task.await??;
    Ok(())
}
