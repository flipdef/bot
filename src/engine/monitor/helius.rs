// src/engine/monitor/helius.rs
// Monitor real de Pump.fun con WS y fallback a polling (solo correcciones mínimas)

use futures::StreamExt;

use std::{
    collections::{HashMap, VecDeque, HashSet},
    env,
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bs58;
use solana_client::{
    nonblocking::{
        pubsub_client::PubsubClient,
        rpc_client::RpcClient as NbRpcClient,
    },
    rpc_client::GetConfirmedSignaturesForAddress2Config,
    rpc_config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter},
    rpc_response::RpcLogsResponse,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Signature,
};
use solana_transaction_status::{
    EncodedTransaction,
    UiMessage,
    UiTransactionEncoding,
    option_serializer::OptionSerializer,
};

use crate::{
    common::logger::Logger,
    dex::pump_fun::{Pump, PUMP_PROGRAM},
    engine::monitor::auto_snipe::handle_create_event_fast,
};
use super::creator_checks::{fetch_creator_stats, CreatorStats};

// === Helpers ENV ===
fn env_usize(key: &str, default: usize) -> usize {
    env::var(key).ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(default)
}
fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
}
fn env_duration_ms(key: &str, default_ms: u64) -> Duration {
    Duration::from_millis(env_u64(key, default_ms))
}
fn ws_url_from_env() -> String {
    if let Ok(ws) = env::var("RPC_WSS") { return ws; }
    let http = env::var("RPC_HTTPS").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    http.replace("https://", "wss://").replace("http://", "ws://")
}

// === LRU TTL Cache ===
struct LruTtlCache<K, V> {
    map: HashMap<K, (V, Instant)>,
    order: VecDeque<K>,
    cap: usize,
    ttl: Duration,
}
impl<K: Eq + Hash + Clone, V: Clone> LruTtlCache<K, V> {
    fn new(cap: usize, ttl: Duration) -> Self { Self { map: HashMap::new(), order: VecDeque::new(), cap, ttl } }
    fn touch(&mut self, k: &K) {
        self.order.retain(|x| x != k);
        self.order.push_back(k.clone());
        if let Some((_, t)) = self.map.get_mut(k) { *t = Instant::now(); }
    }
    fn remove(&mut self, k: &K) {
        self.map.remove(k);
        self.order.retain(|x| x != k);
    }
    fn insert(&mut self, k: K, v: V) {
        if self.map.contains_key(&k) {
            if let Some((val, t)) = self.map.get_mut(&k) { *val = v; *t = Instant::now(); self.touch(&k); return; }
        }
        if self.order.len() >= self.cap { if let Some(old) = self.order.pop_front() { self.map.remove(&old); } }
        self.order.push_back(k.clone());
        self.map.insert(k, (v, Instant::now()));
    }
    fn get(&mut self, k: &K) -> Option<V> {
        let now = Instant::now();
        let within = match self.map.get(k) { Some((_, t)) => now.duration_since(*t) <= self.ttl, None => return None };
        if within { let val = self.map.get(k).map(|(v, _)| v.clone()); self.touch(k); val } else { self.remove(k); None }
    }
}

// === Monitor con cache ===
pub struct HeliusMonitor {
    rpc: Arc<NbRpcClient>,
    cache: LruTtlCache<Pubkey, CreatorStats>,
    scan_limit: usize,
}
impl HeliusMonitor {
    pub fn new(rpc: Arc<NbRpcClient>) -> Self {
        let cap = env_usize("CREATOR_STATS_CACHE_CAP", 1024);
        let ttl = env_duration_ms("CREATOR_STATS_CACHE_TTL_MS", 45_000);
        let scan_limit = env_usize("CREATOR_SCAN_LIMIT", 200);
        Self { rpc, cache: LruTtlCache::new(cap, ttl), scan_limit }
    }
    pub async fn on_pump_create_event(&mut self, creator: Pubkey, mint: Pubkey) -> Result<CreatorStats> {
        if let Some(stats) = self.cache.get(&creator) { return Ok(stats); }
        let stats = fetch_creator_stats(Arc::clone(&self.rpc), creator, mint, self.scan_limit)
            .await
            .with_context(|| format!("fetch_creator_stats for creator={creator} mint={mint}"))?;
        self.cache.insert(creator, stats.clone());
        Ok(stats)
    }
}

/// Loop WebSocket (corregido para 1.18.x): crear cliente y luego subscribir
pub async fn run_ws_monitor(
    rpc_nb: Arc<NbRpcClient>,
    pump: Arc<Pump>,
    logger: &Logger,
) -> Result<()> {
    let ws_url = ws_url_from_env();
    logger.log(format!("🔌 Conectando WS: {}", ws_url));

    // 1) Crear cliente WS
    let client = PubsubClient::new(ws_url.as_str()).await?;

    // 2) Suscribirse a logs del programa Pump.fun
    let filter = RpcTransactionLogsFilter::Mentions(vec![PUMP_PROGRAM.to_string()]);
    let config = RpcTransactionLogsConfig { commitment: Some(CommitmentConfig::processed()) };
    let (mut subscription, _unsubscribe) = client.logs_subscribe(filter, config).await?;

    logger.log("✅ WS suscrito a logs de Pump.fun (create/buy/sell)".to_string());

    while let Some(message) = StreamExt::next(&mut subscription).await {
        let RpcLogsResponse { signature, logs: _logs, err, .. } = message.value;
        if err.is_some() { continue; }

        let Ok(sig) = signature.parse::<Signature>() else { continue };
        let Ok(tx) = rpc_nb.get_transaction(&sig, UiTransactionEncoding::Json).await else { continue };
        let inner = tx.transaction;
        let enc_tx = inner.transaction;

        match &enc_tx {
            EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                UiMessage::Raw(raw) => {
                    let Some(creator) = raw.account_keys.first().and_then(|s| s.parse::<Pubkey>().ok()) else { continue };
                    let disc_create = {
                        let h = solana_sdk::hash::hashv(&[b"global:create".as_slice()]);
                        let mut out = [0u8; 8]; out.copy_from_slice(&h.to_bytes()[..8]); out
                    };
                    let mut found_mint: Option<Pubkey> = None;
                    for ix in &raw.instructions {
                        let Some(prog_id) = raw.account_keys.get(ix.program_id_index as usize) else { continue };
                        if prog_id != &PUMP_PROGRAM { continue; }
                        if let Ok(bytes) = bs58::decode(&ix.data).into_vec() {
                            if bytes.len() >= 8 && bytes[..8] == disc_create {
                                if let Some(&acc0) = ix.accounts.first() {
                                    found_mint = raw.account_keys.get(acc0 as usize).and_then(|s| s.parse::<Pubkey>().ok());
                                }
                                break;
                            }
                        }
                    }
                    if let Some(mint) = found_mint {
                        logger.log(format!("[WS CREATE] mint={mint} creator={creator} sig={sig}"));
                        let _ = handle_create_event_fast(rpc_nb.clone(), pump.clone(), mint, creator, logger).await;
                    }
                }
                UiMessage::Parsed(parsed) => {
                    let creator = parsed.account_keys.first().and_then(|k| k.pubkey.parse().ok());
                    let mint = parsed.account_keys.get(0).and_then(|k| k.pubkey.parse().ok());
                    if let (Some(mint), Some(creator)) = (mint, creator) {
                        logger.log(format!("[WS CREATE/logs] mint={mint} creator={creator} sig={sig}"));
                        let _ = handle_create_event_fast(rpc_nb.clone(), pump.clone(), mint, creator, logger).await;
                    }
                }
            },
            _ => {}
        }
    }

    Ok(())
}

/// Loop de polling (fallback)
pub async fn run_monitor_loop(
    rpc: Arc<NbRpcClient>,
    pump: Arc<Pump>,
    logger: &Logger,
) -> Result<()> {
    use tokio::time::sleep;

    let pump_program: Pubkey = PUMP_PROGRAM.parse().context("PUMP_PROGRAM inválido")?;

    fn anchor_disc(name: &str) -> [u8; 8] {
        let h = solana_sdk::hash::hashv(&[format!("global:{}", name).as_bytes()]);
        let mut out = [0u8; 8];
        out.copy_from_slice(&h.to_bytes()[..8]);
        out
    }
    let disc_create = anchor_disc("create");

    let mut seen: VecDeque<Signature> = VecDeque::new();
    let mut seen_set: HashSet<Signature> = HashSet::new();
    let max_seen = 10_000usize;

    logger.log("🛰️ run_monitor_loop: escuchando transacciones Pump.fun".to_string());

    loop {
        let sigs = rpc
            .get_signatures_for_address_with_config(
                &pump_program,
                GetConfirmedSignaturesForAddress2Config { limit: Some(200), ..Default::default() },
            )
            .await
            .context("get_signatures_for_address(PUMP_PROGRAM)")?;

        for s in sigs.into_iter().rev() {
            let Ok(sig) = s.signature.parse::<Signature>() else { continue };
            if seen_set.contains(&sig) { continue; }

            let Ok(tx) = rpc.get_transaction(&sig, UiTransactionEncoding::Json).await else { continue };
            let inner = tx.transaction;
            let meta_opt = inner.meta.clone();
            let enc_tx = inner.transaction;

            match &enc_tx {
                EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                    UiMessage::Raw(raw) => {
                        let Some(creator_pk) = raw.account_keys.first().and_then(|s| s.parse::<Pubkey>().ok()) else { continue };
                        let mut found_mint: Option<Pubkey> = None;
                        for ix in &raw.instructions {
                            let Some(prog_id) = raw.account_keys.get(ix.program_id_index as usize) else { continue };
                            if prog_id != &PUMP_PROGRAM { continue; }
                            if let Ok(bytes) = bs58::decode(&ix.data).into_vec() {
                                if bytes.len() >= 8 && bytes[..8] == disc_create {
                                    if let Some(&acc0) = ix.accounts.first() {
                                        found_mint = raw.account_keys.get(acc0 as usize).and_then(|s| s.parse::<Pubkey>().ok());
                                    }
                                    break;
                                }
                            }
                        }
                        if let Some(mint) = found_mint {
                            logger.log(format!("[CREATE/raw] mint={mint} creator={creator_pk}"));
                            let _ = handle_create_event_fast(rpc.clone(), pump.clone(), mint, creator_pk, logger).await;
                        }
                    }
                    UiMessage::Parsed(parsed) => {
                        if let (Some(creator_pk), Some(meta)) = (parsed.account_keys.first().and_then(|k| k.pubkey.parse().ok()), meta_opt) {
                            match meta.log_messages {
                                OptionSerializer::Some(logs) => {
                                    if logs.iter().any(|l| l.contains("Instruction: Create")) {
                                        let mint = parsed.account_keys.get(0).and_then(|k| k.pubkey.parse().ok());
                                        if let Some(mint) = mint {
                                            logger.log(format!("[CREATE/logs] mint={mint} creator={creator_pk}"));
                                            let _ = handle_create_event_fast(rpc.clone(), pump.clone(), mint, creator_pk, logger).await;
                                        }
                                    }
                                }
                                OptionSerializer::None => {}
                                OptionSerializer::Skip => {}
                            }
                        }
                    }
                },
                _ => {}
            }

            seen_set.insert(sig);
            seen.push_back(sig);
            if seen.len() > max_seen { if let Some(old) = seen.pop_front() { seen_set.remove(&old); } }
        }

        sleep(Duration::from_millis(800)).await;
    }
}
