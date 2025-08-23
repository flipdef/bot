use std::env;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use std::str::FromStr;
use std::time::Instant;

use crate::{
    common::{logger::Logger, rpc},
    services::zeroslot,
};

// prioritization fee = UNIT_PRICE * UNIT_LIMIT
fn get_unit_price() -> u64 {
    env::var("UNIT_PRICE")
        .ok()
        .and_then(|v| u64::from_str(&v).ok())
        .unwrap_or(1)
}

fn get_unit_limit() -> u32 {
    env::var("UNIT_LIMIT")
        .ok()
        .and_then(|v| u32::from_str(&v).ok())
        .unwrap_or(300_000)
}

pub async fn new_signed_and_send(
    client: &RpcClient,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    use_zeroslot: bool,
    logger: &Logger,
) -> Result<Vec<String>> {
    let unit_price = get_unit_price();
    let unit_limit = get_unit_limit();

    // Si no usamos 0slot → añadimos manualmente las instrucciones de compute budget
    if !use_zeroslot {
        let modify_compute_units =
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(
                unit_price,
            );
        let add_priority_fee =
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(
                unit_limit,
            );
        instructions.insert(0, modify_compute_units);
        instructions.insert(1, add_priority_fee);
    }

    // Crear transacción base
    let recent_blockhash = client.get_latest_blockhash()?;
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        recent_blockhash,
    );

    let start_time = Instant::now();
    let mut txs = vec![];

    if use_zeroslot {
        // === Usando 0slot ===
        let sig = zeroslot::send_transaction_zeroslot(&txn).await?;
        logger.log(format!("0slot signature: {}", sig));
        txs.push(sig.to_string());
    } else {
        // === RPC normal ===
        let sig = rpc::send_txn(client, &txn, true)?;
        logger.log(format!("signature: {:#?}", sig));
        txs.push(sig.to_string());
    }

    logger.log(format!("tx elapsed: {:?}", start_time.elapsed()));
    Ok(txs)
}
