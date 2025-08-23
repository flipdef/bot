// src/engine/monitor/creator_checks.rs
// Heurísticas ultra-rápidas para filtrar creadores en pump.fun
// - Cuenta cuántas monedas ha creado un `creator` (ix `create`)
// - Suma cuánto SOL compró el `creator` del `mint` objetivo (ix `buy`)
// Llamar justo tras `CreateEvent` desde el monitor.

use std::{
    collections::HashSet,
    sync::{atomic::{AtomicU64, Ordering}, Arc},
};

use anyhow::{Context, Result};
use futures::{stream, StreamExt};
use solana_client::nonblocking::rpc_client::RpcClient as NbRpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config; // v1.18.x
use solana_sdk::{hash::hashv, pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta,
    EncodedTransaction,
    UiCompiledInstruction,
    UiMessage,
    UiTransactionEncoding,
};
use tokio::sync::Mutex;

use crate::dex::pump_fun::PUMP_PROGRAM;

#[derive(Debug, Clone)]
pub struct CreatorStats {
    pub creator: Pubkey,
    pub created_mints: Vec<Pubkey>,
    pub total_buys_lamports: u64,
}

/// Anchor: sha256("global:" + name)[0..8]
fn anchor_discriminator(name: &str) -> [u8; 8] {
    let s = format!("global:{}", name);
    let h = hashv(&[s.as_bytes()]);
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

fn disc_create() -> [u8; 8] { anchor_discriminator("create") }
fn disc_buy() -> [u8; 8] { anchor_discriminator("buy") }

fn decode_ix_data_bs58(ix: &UiCompiledInstruction) -> Option<Vec<u8>> {
    bs58::decode(&ix.data).into_vec().ok()
}

fn extract_account_keys(enc_tx: &EncodedTransaction) -> Vec<Pubkey> {
    match enc_tx {
        EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            UiMessage::Parsed(parsed) => parsed
                .account_keys
                .iter()
                .filter_map(|k| k.pubkey.parse().ok())
                .collect(),
            UiMessage::Raw(raw) => raw
                .account_keys
                .iter()
                .filter_map(|k| k.parse().ok())
                .collect(),
        },
        _ => vec![],
    }
}

pub async fn fetch_creator_stats(
    rpc: Arc<NbRpcClient>,
    creator: Pubkey,
    target_mint: Pubkey,
    limit: usize,
) -> Result<CreatorStats> {
    // 1) Firmas recientes del creador
    let sigs = rpc
        .get_signatures_for_address_with_config(
            &creator,
            GetConfirmedSignaturesForAddress2Config {
                limit: Some(limit.min(1000)),
                ..Default::default()
            },
        )
        .await
        .context("get_signatures_for_address(creator)")?;

    // 2) Descarga transacciones en paralelo (UiMessage::Raw para `data` base58)
    let fetches = sigs
        .into_iter()
        .filter_map(|x| x.signature.parse::<Signature>().ok())
        .map(|sig| {
            let rpc = rpc.clone();
            async move {
                let tx: Option<EncodedConfirmedTransactionWithStatusMeta> =
                    rpc.get_transaction(&sig, UiTransactionEncoding::Json)
                        .await
                        .ok();
                (sig, tx)
            }
        });

    // Acumuladores thread-safe para concurrencia
    let created: Arc<Mutex<HashSet<Pubkey>>> = Arc::new(Mutex::new(HashSet::new()));
    let total_buys_lamports = Arc::new(AtomicU64::new(0));

    let create8 = disc_create();
    let buy8 = disc_buy();
    let pump_program = PUMP_PROGRAM.parse::<Pubkey>().expect("PUMP_PROGRAM");

    stream::iter(fetches)
        .buffer_unordered(32)
        .for_each({
            let created = Arc::clone(&created);
            let total_buys_lamports = Arc::clone(&total_buys_lamports);
            let creator = creator; // Copy
            move |(_sig, tx)| {
                let target_mint = target_mint; // Copy
                let created = Arc::clone(&created);
                let total_buys_lamports = Arc::clone(&total_buys_lamports);
                async move {
                    let Some(tx) = tx else { return }; // fallo RPC
                    let inner = tx.transaction;         // EncodedTransactionWithStatusMeta
                    let Some(meta) = inner.meta else { return };
                    let enc_tx = inner.transaction;     // EncodedTransaction

                    let account_keys = extract_account_keys(&enc_tx);
                    if account_keys.is_empty() { return; }

                    let Some(creator_idx) = account_keys.iter().position(|k| k == &creator) else { return };

                    let pre = meta.pre_balances;
                    let post = meta.post_balances;
                    let fee = meta.fee;
                    if pre.len() != post.len() || creator_idx >= pre.len() { return; }

                    let mut is_buy_of_target = false;

                    if let EncodedTransaction::Json(ui_tx) = &enc_tx {
                        if let UiMessage::Raw(raw) = &ui_tx.message {
                            for ix in &raw.instructions {
                                let Some(prog) = account_keys.get(ix.program_id_index as usize) else { continue };
                                if prog != &pump_program { continue; }
                                if let Some(bytes) = decode_ix_data_bs58(ix) {
                                    if bytes.len() >= 8 {
                                        if bytes[..8] == create8 {
                                            if let Some(&acc0) = ix.accounts.first() {
                                                if let Some(pk) = account_keys.get(acc0 as usize) {
                                                    created.lock().await.insert(*pk);
                                                }
                                            }
                                        } else if bytes[..8] == buy8 {
                                            let mut mentions_target = false;
                                            for &acc_idx in &ix.accounts {
                                                if let Some(pk) = account_keys.get(acc_idx as usize) {
                                                    if *pk == target_mint { mentions_target = true; break; }
                                                }
                                            }
                                            if mentions_target { is_buy_of_target = true; }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if is_buy_of_target {
                        let mut spent = pre[creator_idx].saturating_sub(post[creator_idx]);
                        // Si el creator es fee payer, descuenta fee para gasto neto
                        if account_keys.first() == account_keys.get(creator_idx) { spent = spent.saturating_sub(fee); }
                        if spent > 0 { total_buys_lamports.fetch_add(spent as u64, Ordering::Relaxed); }
                    }
                }
            }
        })
        .await;

    let created_mints: Vec<Pubkey> = {
        let set = created.lock().await;
        set.iter().copied().collect()
    };
    let total_buys_lamports = total_buys_lamports.load(Ordering::Relaxed);

    Ok(CreatorStats { creator, created_mints, total_buys_lamports })
}

pub fn should_snipe(stats: &CreatorStats, max_prev_creations: usize, min_creator_buy_lamports: u64) -> bool {
    (stats.created_mints.len() <= max_prev_creations) && (stats.total_buys_lamports >= min_creator_buy_lamports)
}
