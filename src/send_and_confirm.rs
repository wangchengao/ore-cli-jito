use std::thread::sleep;
use std::time::Duration;

use colored::*;
use drillx::difficulty;
use rand::Rng;
use solana_client::{
    client_error::{ClientError, ClientErrorKind, Result as ClientResult},
    rpc_config::RpcSendTransactionConfig,
};
use solana_program::{
    instruction::Instruction,
    instruction::AccountMeta,
    native_token::{lamports_to_sol, sol_to_lamports},
};
use solana_rpc_client::spinner;
use solana_sdk::{
    commitment_config::CommitmentLevel,
    compute_budget::ComputeBudgetInstruction,
    signature::{Signature, Signer},
    transaction::Transaction,
    pubkey,
};
use solana_transaction_status::{Encodable, EncodedTransaction, TransactionConfirmationStatus, UiTransactionEncoding};
use solana_program::pubkey::Pubkey;

use serde::{Deserialize, Serialize};

use serde_json::{json, Value};
use eyre::{Report, Result};
use futures::future::ok;
use reqwest;
use reqwest::header::{HeaderMap, CONTENT_TYPE};

use crate::Miner;

const MIN_SOL_BALANCE: f64 = 0.005;

const RPC_RETRIES: usize = 0;
const _SIMULATION_RETRIES: usize = 4;
const GATEWAY_RETRIES: usize = 20;
const CONFIRM_RETRIES: usize = 1;

const CONFIRM_DELAY: u64 = 0;
const GATEWAY_DELAY: u64 = 300;

pub enum ComputeBudget {
    Dynamic,
    Fixed(u32),
}

pub const JITO_RECIPIENTS: [Pubkey; 8] = [
    // mainnet
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];


// jito submit
#[derive(Deserialize, Serialize, Debug)]
struct BundleSendResponse {
    id: u64,
    jsonrpc: String,
    result: String,
}

// jito query

#[derive(Serialize, Deserialize, Debug)]
struct BundleStatusResponse {
    jsonrpc: String,
    result: BundleStatusResult,
    id: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct BundleStatusResult {
    context: Context,
    value: Vec<BundleStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Context {
    slot: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct BundleStatus {
    bundle_id: String,
    transactions: Vec<String>,
    slot: u64,
    confirmation_status: String,
    err: Option<serde_json::Value>,
}

async fn get_bundle_statuses(params: Value) -> Result<BundleStatusResponse> {
    let client = reqwest::Client::new();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
    let response = client
        .post("https://mainnet.block-engine.jito.wtf:443/api/v1/bundles")
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": params
        }))
        .send()
        .await
        .map_err(|err| eyre::eyre!("Failed to send request: {err}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| eyre::eyre!("Failed to read response content: {err:#}"))?;

    if !status.is_success() {
        eyre::bail!("Status code: {status}, response: {text}");
    }

    return serde_json::from_str(&text)
        .map_err(|err| eyre::eyre!("Failed to deserialize response: {err:#}, response: {text}, status: {status}"));
}


async fn send_jito_bundle(params: Value) -> Result<BundleSendResponse> {
    let client = reqwest::Client::new();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());

    let response = client
        .post("https://mainnet.block-engine.jito.wtf:443/api/v1/bundles")
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": params
        }))
        .send()
        .await
        .map_err(|err| eyre::eyre!("Failed to send request: {err}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| eyre::eyre!("Failed to read response content: {err:#}"))?;

    if !status.is_success() {
        eyre::bail!("Status code: {status}, response: {text}");
    }

    return serde_json::from_str(&text)
        .map_err(|err| eyre::eyre!("Failed to deserialize response: {err:#}, response: {text}, status: {status}"));
}

pub fn build_bribe_ix(pubkey: &Pubkey, value: u64) -> solana_sdk::instruction::Instruction {
    solana_sdk::system_instruction::transfer(pubkey, &JITO_RECIPIENTS[rand::thread_rng().gen_range(0..JITO_RECIPIENTS.len())], value)
}

impl Miner {
    pub async fn send_and_confirm(
        &self,
        ixs: &[Instruction],
        compute_budget: ComputeBudget,
        skip_confirm: bool,
    ) -> ClientResult<Signature> {
        let progress_bar = spinner::new_progress_bar();
        let signer = self.signer();
        let client = self.rpc_client.clone();

        // Return error, if balance is zero
        if let Ok(balance) = client.get_balance(&signer.pubkey()).await {
            if balance <= sol_to_lamports(MIN_SOL_BALANCE) {
                panic!(
                    "{} Insufficient balance: {} SOL\nPlease top up with at least {} SOL",
                    "ERROR".bold().red(),
                    lamports_to_sol(balance),
                    MIN_SOL_BALANCE
                );
            }
        }

        // Set compute units
        let mut final_ixs = vec![];
        match compute_budget {
            ComputeBudget::Dynamic => {
                // TODO simulate
                final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(1_400_000))
            }
            ComputeBudget::Fixed(cus) => {
                final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(cus))
            }
        }
        final_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
            self.priority_fee,
        ));
        final_ixs.extend_from_slice(ixs);

        // Build tx
        let send_cfg = RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            encoding: Some(UiTransactionEncoding::Base64),
            max_retries: Some(RPC_RETRIES),
            min_context_slot: None,
        };
        let mut tx = Transaction::new_with_payer(&final_ixs, Some(&signer.pubkey()));

        // Sign tx
        let (hash, _slot) = client
            .get_latest_blockhash_with_commitment(self.rpc_client.commitment())
            .await
            .unwrap();
        tx.sign(&[&signer], hash);

        // Submit tx
        let mut attempts = 0;
        loop {
            progress_bar.set_message(format!("Submitting transaction... (attempt {})", attempts));
            match client.send_transaction_with_config(&tx, send_cfg).await {
                Ok(sig) => {
                    // Skip confirmation
                    if skip_confirm {
                        progress_bar.finish_with_message(format!("Sent: {}", sig));
                        return Ok(sig);
                    }

                    // Confirm the tx landed
                    for _ in 0..CONFIRM_RETRIES {
                        std::thread::sleep(Duration::from_millis(CONFIRM_DELAY));
                        match client.get_signature_statuses(&[sig]).await {
                            Ok(signature_statuses) => {
                                for status in signature_statuses.value {
                                    if let Some(status) = status {
                                        if let Some(err) = status.err {
                                            progress_bar.finish_with_message(format!(
                                                "{}: {}",
                                                "ERROR".bold().red(),
                                                err
                                            ));
                                            return Err(ClientError {
                                                request: None,
                                                kind: ClientErrorKind::Custom(err.to_string()),
                                            });
                                        }
                                        if let Some(confirmation) = status.confirmation_status {
                                            match confirmation {
                                                TransactionConfirmationStatus::Processed => {}
                                                TransactionConfirmationStatus::Confirmed
                                                | TransactionConfirmationStatus::Finalized => {
                                                    progress_bar.finish_with_message(format!(
                                                        "{} {}",
                                                        "OK".bold().green(),
                                                        sig
                                                    ));
                                                    return Ok(sig);
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Handle confirmation errors
                            Err(err) => {
                                progress_bar.set_message(format!(
                                    "{}: {}",
                                    "ERROR".bold().red(),
                                    err.kind().to_string()
                                ));
                            }
                        }
                    }
                }

                // Handle submit errors
                Err(err) => {
                    progress_bar.set_message(format!(
                        "{}: {}",
                        "ERROR".bold().red(),
                        err.kind().to_string()
                    ));
                }
            }

            // Retry
            std::thread::sleep(Duration::from_millis(GATEWAY_DELAY));
            attempts += 1;
            if attempts > GATEWAY_RETRIES {
                progress_bar.finish_with_message(format!("{}: Max retries", "ERROR".bold().red()));
                return Err(ClientError {
                    request: None,
                    kind: ClientErrorKind::Custom("Max retries".into()),
                });
            }
        }
    }

    // TODO
    fn _simulate(&self) {

        // Simulate tx
        // let mut sim_attempts = 0;
        // 'simulate: loop {
        //     let sim_res = client
        //         .simulate_transaction_with_config(
        //             &tx,
        //             RpcSimulateTransactionConfig {
        //                 sig_verify: false,
        //                 replace_recent_blockhash: true,
        //                 commitment: Some(self.rpc_client.commitment()),
        //                 encoding: Some(UiTransactionEncoding::Base64),
        //                 accounts: None,
        //                 min_context_slot: Some(slot),
        //                 inner_instructions: false,
        //             },
        //         )
        //         .await;
        //     match sim_res {
        //         Ok(sim_res) => {
        //             if let Some(err) = sim_res.value.err {
        //                 println!("Simulaton error: {:?}", err);
        //                 sim_attempts += 1;
        //             } else if let Some(units_consumed) = sim_res.value.units_consumed {
        //                 if dynamic_cus {
        //                     println!("Dynamic CUs: {:?}", units_consumed);
        //                     let cu_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(
        //                         units_consumed as u32 + 1000,
        //                     );
        //                     let cu_price_ix =
        //                         ComputeBudgetInstruction::set_compute_unit_price(self.priority_fee);
        //                     let mut final_ixs = vec![];
        //                     final_ixs.extend_from_slice(&[cu_budget_ix, cu_price_ix]);
        //                     final_ixs.extend_from_slice(ixs);
        //                     tx = Transaction::new_with_payer(&final_ixs, Some(&signer.pubkey()));
        //                 }
        //                 break 'simulate;
        //             }
        //         }
        //         Err(err) => {
        //             println!("Simulaton error: {:?}", err);
        //             sim_attempts += 1;
        //         }
        //     }

        //     // Abort if sim fails
        //     if sim_attempts.gt(&SIMULATION_RETRIES) {
        //         return Err(ClientError {
        //             request: None,
        //             kind: ClientErrorKind::Custom("Simulation failed".into()),
        //         });
        //     }
        // }
    }

    pub async fn send_and_confirm_jito(
        &self,
        ixs: &[Instruction],
        compute_budget: ComputeBudget,
        skip_confirm: bool,
        difficult: u32,
    ) -> (Result<Value> ){
        let progress_bar = spinner::new_progress_bar();
        let signer = self.signer();
        let client = self.rpc_client.clone();

        // Return error, if balance is zero
        if let Ok(balance) = client.get_balance(&signer.pubkey()).await {
            if balance <= sol_to_lamports(MIN_SOL_BALANCE) {
                panic!(
                    "{} Insufficient balance: {} SOL\nPlease top up with at least {} SOL",
                    "ERROR".bold().red(),
                    lamports_to_sol(balance),
                    MIN_SOL_BALANCE
                );
            }
        }

        // Set compute units
        let mut final_ixs = vec![];
        match compute_budget {
            ComputeBudget::Dynamic => {
                // TODO simulate
                final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(1_400_000))
            }
            ComputeBudget::Fixed(cus) => {
                final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(cus))
            }
        }
        final_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
            0,
        ));

        final_ixs.push(build_bribe_ix(&signer.pubkey(),  self.adjust_fee(difficult)));

        final_ixs.extend_from_slice(ixs);

        let mut tx = Transaction::new_with_payer(&final_ixs, Some(&signer.pubkey()));

        // Sign tx
        let (hash, _slot) = client
            .get_latest_blockhash_with_commitment(self.rpc_client.commitment())
            .await
            .unwrap();
        tx.sign(&[&signer], hash);


        let mut bundle = Vec::with_capacity(5);
        bundle.push(tx);

        let signature = *bundle
            .first()
            .expect("empty bundle")
            .signatures
            .first()
            .expect("empty transaction");

        let bundle = bundle
            .into_iter()
            .map(|tx| match tx.encode(UiTransactionEncoding::Binary) {
                EncodedTransaction::LegacyBinary(b) => b,
                _ => panic!("encode-jito-tx-err"),
            })
            .collect::<Vec<_>>();


        let mut attempts = 0;
        let mut bundle_id = String::new();
        ;
        progress_bar.set_message(format!("Submitting jito transaction... (attempt {})", attempts));
        match send_jito_bundle(json!([bundle])).await {
            Ok(sig) => {
                bundle_id = sig.result.clone();
                progress_bar.finish_with_message(format!("Sent: {:?}", sig));
            }
            // Handle submit errors
            Err(err) => {
                progress_bar.set_message(format!(
                    "{}: {}",
                    "ERROR".bold().red(),
                    err.to_string()
                ));
                return Err(Report::from(ClientError {
                    request: None,
                    kind: ClientErrorKind::Custom("submit bundle err".into()),
                }));
            }
        }

        loop {
            println!("query bundle bundle_id: {}, attempts: {}", bundle_id, attempts);

            // Retry
            let param = bundle_id.clone();
            match get_bundle_statuses(json!([[param]])).await {
                Ok(resp) => {
                    if resp.result.value.len() > 0 && resp.result.value[0].confirmation_status == "confirmed" {
                        println!(" Bundle landed successfully");
                        println!(" https://solscan.io/tx/{}?cluster={}", resp.result.value[0].transactions[0], "mainnet");
                        return Ok(Value::from(()));
                    }
                }
                // Handle submit errors
                Err(err) => {
                    println!("{}", err);
                }
            }
            std::thread::sleep(Duration::from_millis(2000));
            attempts += 1;
            if attempts > GATEWAY_RETRIES {
                progress_bar.finish_with_message(format!("{}: Max retries", "ERROR".bold().red()));
                return Err(Report::from(ClientError {
                    request: None,
                    kind: ClientErrorKind::Custom("Max retries".into()),
                }));
            }
        }
    }

    pub fn adjust_fee(&self, difficult: u32) -> u64 {
        let mut extra_fee = self.priority_fee.clone();
        if difficult > 20 {
            extra_fee += (extra_fee as f64 * (difficult as f64 - 20f64) / 20f64) as u64 * 4;
            // 约束下上限
            if extra_fee > 3 * self.priority_fee {
                extra_fee = 3 * self.priority_fee
            }
            println!("change extra fee to {}", extra_fee)
        }
        return extra_fee
    }
}

