use bitcoin::consensus::serialize;
use bitcoin::consensus::Decodable;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::util::bip32::ExtendedPrivKey;
use bitcoin::util::bip32::ExtendedPubKey;
use bitcoin::util::psbt::PartiallySignedTransaction;
use bitcoin::Amount;
use bitcoin::Denomination;
use bitcoin::OutPoint;
use bitcoincore_rpc_async as rpc;
use bitcoincore_rpc_async::RpcApi;
use clap::clap_app;
use config::*;
use emulator_connect::servers::hd::HDOracleEmulator;
use emulator_connect::CTVEmulator;
use emulator_connect::NullEmulator;
use sapio::contract::Compiled;
use sapio_base::txindex::TxIndex;
use sapio_base::txindex::TxIndexLogger;
use sapio_base::util::CTVHash;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use util::*;

pub mod config;
pub mod prixfixe;
mod util;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = clap_app!("sapio-cli" =>
        (@setting SubcommandRequiredElseHelp)
        (version: "0.1.0 Beta")
        (author: "Jeremy Rubin <j@rubin.io>")
        (about: "Sapio CLI for Bitcoin Smart Contracts")
        (@arg config: -c --config +takes_value #{1,2} {check_file} "Sets a custom config file")
        (@arg debug: -d ... "Sets the level of debugging information")
        (@subcommand emulator =>
            (about: "Make Requests to Emulator Servers")
            (@subcommand sign =>
                (about: "Sign a PSBT")
                (@arg psbt: -p --psbt +takes_value +required #{1,2} {check_file} "The file containing the PSBT to Sign")
                (@arg out: -o --output +takes_value +required #{1,2} {check_file_not} "The file to save the resulting PSBT")
            )
            (@subcommand get_key =>
                (about: "Get Signing Condition")
                (@arg psbt: -p --psbt +takes_value +required #{1,2} {check_file} "The file containing the PSBT to Get a Key For")
            )
            (@subcommand show =>
                (about: "Show a psbt")
                (@arg psbt: -p --psbt +takes_value +required #{1,2} {check_file} "The file containing the PSBT to Get a Key For")
            )
            (@subcommand server =>
                (about: "run an emulation server")
                (@arg sync: --sync  "Run in Synchronous mode")
                (@arg seed: +takes_value +required {check_file} "The file containing the Seed")
                (@arg interface: +required +takes_value "The Interface to Bind")
            )
        )
        (@subcommand contract =>
            (about: "Create or Manage a Contract")
            (@subcommand bind =>
                (about: "Bind Contract to a specific UTXO")
                (@arg json: +required "JSON to Bind")
            )
            (@subcommand create =>
                (@arg name: +required "Which Contract to Create")
                (@arg amount: +required "Amount to Send in BTC")
                (@arg params: +required "JSON of args")
                (about: "create a contract to a specific UTXO")
            )
            (@subcommand list =>
                (about: "list available contracts")
            )
        )
    )
    .get_matches();

    let config = Config::setup(&matches, "org", "judica", "sapio-cli").await?;

    let cfg = config.active;
    let emulator = NullEmulator(if let Some(emcfg) = &cfg.emulator_nodes {
        if emcfg.enabled {
            Some(emcfg.get_emulator()?.into())
        } else {
            None
        }
    } else {
        None
    });

    {
        let mut emulator = emulator.clone();
        // Drop Emulator from own thread...
        std::thread::spawn(move || loop {
            if let Some(_) = emulator.0.as_mut().and_then(|e| Arc::get_mut(e)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        });
    }
    match matches.subcommand() {
        Some(("emulator", sign_matches)) => match sign_matches.subcommand() {
            Some(("sign", args)) => {
                let psbt = decode_psbt_file(args, "psbt")?;
                let psbt = emulator.sign(psbt)?;
                let bytes = serialize(&psbt);
                std::fs::write(args.value_of_os("out").unwrap(), &base64::encode(bytes))?;
            }
            Some(("get_key", args)) => {
                let psbt = decode_psbt_file(args, "psbt")?;
                let h = emulator.get_signer_for(psbt.extract_tx().get_ctv_hash(0))?;
                println!("{}", h);
            }
            Some(("show", args)) => {
                let psbt = decode_psbt_file(args, "psbt")?;
                println!("{:?}", psbt);
            }
            Some(("server", args)) => {
                let filename = args.value_of("seed").unwrap();
                let contents = tokio::fs::read(filename).await?;

                let root = ExtendedPrivKey::new_master(config.network, &contents[..]).unwrap();
                let pk_root = ExtendedPubKey::from_private(&Secp256k1::new(), &root);
                let oracle = HDOracleEmulator::new(root, args.is_present("sync"));
                let server = oracle.bind(args.value_of("interface").unwrap());
                println!("Running Oracle With Key: {}", pk_root);
                server.await?;
            }
            _ => unreachable!(),
        },
        Some(("contract", matches)) => match matches.subcommand() {
            Some(("list", _args)) => {
                for s in prixfixe::MENU.list() {
                    println!("{}", s)
                }
            }
            Some(("bind", args)) => {
                let client =
                    rpc::Client::new(cfg.api_node.url.clone(), cfg.api_node.auth.clone()).await?;
                let j: Compiled = serde_json::from_str(args.value_of("json").unwrap())?;
                let _out_in = OutPoint::default();
                let mut spends = HashMap::new();
                spends.insert(format!("{}", j.address), j.amount_range.max());
                let res = client
                    .wallet_create_funded_psbt(&[], &spends, None, None, None)
                    .await?;
                let psbt =
                    PartiallySignedTransaction::consensus_decode(&base64::decode(&res.psbt)?[..])?;
                let tx = psbt.extract_tx();
                // if change pos is -1, then +1%len == 0. If it is 0, then 1. If 1, then 2 % len == 0.
                let vout = ((res.change_position + 1) as usize) % tx.output.len();
                let logger = Rc::new(TxIndexLogger::new());
                (*logger).add_tx(Arc::new(tx.clone()))?;

                let out = j.bind_psbt(
                    OutPoint::new(tx.txid(), vout as u32),
                    HashMap::new(),
                    logger,
                    &emulator,
                )?;
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            Some(("create", args)) => {
                let amt =
                    Amount::from_str_in(args.value_of("amount").unwrap(), Denomination::Bitcoin)?;
                let ctx = sapio::contract::Context::new(config.network, amt, emulator.0.clone());
                let contract = prixfixe::MENU.compile(
                    args.value_of("name").unwrap().into(),
                    serde_json::from_str(args.value_of("params").unwrap())?,
                    &ctx,
                )?;
                println!("{}", serde_json::to_string_pretty(&contract)?);
            }
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };

    Ok(())
}
