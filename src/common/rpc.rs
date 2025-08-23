use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::transaction::Transaction;

pub fn send_txn(
    client: &RpcClient,
    txn: &Transaction,
    _commitment: bool,
) -> Result<solana_sdk::signature::Signature> {
    let sig = client.send_and_confirm_transaction(txn)?;
    Ok(sig)
}
