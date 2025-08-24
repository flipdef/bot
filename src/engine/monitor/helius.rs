// src/engine/monitor/helius.rs
// Monitor Pump.fun optimizado para Helius Streams (Enhanced WS):
// - WS usa transactionSubscribe (atlas-mainnet) y NO hace llamadas RPC por tx.
// - Polling queda como fallback.

use futures::{SinkExt, StreamExt};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bs58;
use serde_json::Value;
use solana_client::{
    nonblocking::rpc_client::RpcClient as NbRpcClient,
    rpc_client::GetConfirmedSignaturesForAddress2Config,
    rpc_config::RpcTransactionConfig,
};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedTransaction,
    UiMessage,
    UiTransactionEncoding,
    option_serializer::OptionSerializer,
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

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
fn env_u64(key: &str, default: u64) -> u64 { env::var(key).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(default) }
fn env_duration_ms(key: &str, default_ms: u64) -> Duration { Duration::from_millis(env_u64(key, default_ms)) }
fn env_bool(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
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

// === Utilidades ===
fn anchor_disc(name: &str) -> [u8; 8] {
    let h = solana_sdk::hash::hashv(&[format!("global:{}", name).as_bytes()]);
    let mut out = [0u8; 8];
    out.copy_from_slice(&h.to_bytes()[..8]);
    out
}

fn contains_create(logs: &[String]) -> bool {
    logs.iter().any(|l| {
        let l = l.as_str();
        l.contains("Instruction: Create") || l.contains("Create") || l.contains("CREATE")
    })
}

fn extract_mint_creator_from_encoded(enc_tx: &EncodedTransaction, disc_create: &[u8; 8]) -> Option<(Pubkey, Pubkey)> {
    match enc_tx {
        EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            UiMessage::Raw(raw) => {
                let creator = raw.account_keys.first()?.parse::<Pubkey>().ok()?;
                let mut mint: Option<Pubkey> = None;
                for ix in &raw.instructions {
                    let prog_id = raw.account_keys.get(ix.program_id_index as usize)?;
                    if prog_id != &PUMP_PROGRAM { continue; }
                    if let Ok(bytes) = bs58::decode(&ix.data).into_vec() {
                        if bytes.len() >= 8 && bytes[..8] == *disc_create {
                            if let Some(&acc0) = ix.accounts.first() {
                                mint = raw.account_keys.get(acc0 as usize).and_then(|s| s.parse::<Pubkey>().ok());
                            }
                            break;
                        }
                    }
                }
                mint.map(|m| (m, creator))
            }
            UiMessage::Parsed(parsed) => {
                let creator = parsed.account_keys.first().and_then(|k| k.pubkey.parse().ok())?;
                let mint = parsed.account_keys.get(0).and_then(|k| k.pubkey.parse().ok())?;
                Some((mint, creator))
            }
        },
        _ => None,
    }
}

fn v_as_str(v: &Value) -> Option<&str> { v.as_str() }
fn v_get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in path { cur = cur.get(*k)?; }
    Some(cur)
}

fn decode_b58_prefix8(data_b58: &str) -> Option<[u8; 8]> {
    let bytes = bs58::decode(data_b58).into_vec().ok()?;
    if bytes.len() < 8 { return None; }
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    Some(out)
}

/// Extrae (signature, mint, creator) desde un mensaje JSON de transactionSubscribe (jsonParsed o raw).
/// Extrae (signature, mint, creator) usando el patrón documentado por Helius para pump.fun:
///  - Filtro por log `Instruction: InitializeMint2` en `meta.logMessages`
///  - `accountKeys[0]` = creator, `accountKeys[1]` = mint
fn parse_helius_stream_create(msg: &Value, _disc_create: &[u8; 8]) -> Option<(Signature, Pubkey, Pubkey)> {
    // Helius Streams (transactionNotification)
    // Detecta creación por log SPL: InitializeMint/InitializeMint2
    let result = v_get(msg, &["params", "result"])?;

    // Firma
    let sig_str = result.get("signature").and_then(v_as_str)?;
    let sig = sig_str.parse::<Signature>().ok()?;

    // ¿Es un initialize mint?
    let logs = v_get(result, &["transaction", "meta", "logMessages"])?.as_array()?;
    let is_mint_init = logs
        .iter()
        .filter_map(|l| l.as_str())
        .any(|l| l.contains("Instruction: InitializeMint2") || l.contains("Instruction: InitializeMint"));
    if !is_mint_init { return None; }

    // accountKeys: strings o {pubkey}
    let msg_root = v_get(result, &["transaction", "transaction", "message"]) 
        .or_else(|| v_get(result, &["transaction", "message"]))?;
    let account_keys_v = msg_root.get("accountKeys")?;

    let mut keys: Vec<String> = Vec::new();
    if let Some(arr) = account_keys_v.as_array() {
        for it in arr {
            if let Some(s) = it.as_str() {
                keys.push(s.to_string());
            } else if let Some(obj_pubkey) = it.get("pubkey").and_then(v_as_str) {
                keys.push(obj_pubkey.to_string());
            }
        }
    }
    if keys.len() < 2 { return None; }

    let creator = keys[0].parse::<Pubkey>().ok()?;
    let mint = keys[1].parse::<Pubkey>().ok()?;

    Some((sig, mint, creator))
}

/// WebSocket (Helius Streams): transactionSubscribe → procesa CREATE sin ir a RPC.
pub async fn run_ws_monitor(
    rpc_nb: Arc<NbRpcClient>,
    pump: Arc<Pump>,
    logger: &Logger,
) -> Result<()> {
    let ws_url = ws_url_from_env();
    logger.log(format!("🔌 Conectando WS (Streams): {}", ws_url));
    logger.log(format!("ℹ️  PUMP_PROGRAM={}", PUMP_PROGRAM));

    // Conectar
    let (mut ws, _resp) = connect_async(&ws_url).await.context("connect atlas WS")?;

    // Suscribirse a transacciones que incluyan el programa Pump.fun
    let sub = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "transactionSubscribe",
        "params": [
            {
                "failed": false,
                "vote": false,
                "accountInclude": [PUMP_PROGRAM]
            },
            {
                "commitment": "processed",
                "encoding": "jsonParsed",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0
            }
        ]
    });
    ws.send(WsMessage::Text(sub.to_string())).await.context("send subscribe")?;

    logger.log("✅ WS Streams suscrito (transactionSubscribe accountInclude=PUMP_PROGRAM)".to_string());

    let disc_create = anchor_disc("create");
    let latency_log = env_bool("LATENCY_LOG", true);

    let mut seen_ws: VecDeque<Signature> = VecDeque::with_capacity(20_000);
    let mut seen_ws_set: HashSet<Signature> = HashSet::with_capacity(20_000);
    let max_seen = 20_000usize;

    while let Some(msg) = ws.next().await {
        let t0 = Instant::now();
        let Ok(msg) = msg else { break };
        match msg {
            WsMessage::Text(txt) => {
                // Ignora acks de suscripción sin "params"
                let Ok(json): Result<Value, _> = serde_json::from_str(&txt) else { continue };
                if json.get("method").and_then(|m| m.as_str()) != Some("transactionNotification") {
                    continue;
                }
                if let Some((sig, mint, creator)) = parse_helius_stream_create(&json, &disc_create) {
                    if seen_ws_set.contains(&sig) { continue; }
                    if latency_log { logger.log(format!("[WS CREATE] mint={} creator={} sig={} | total={}ms", mint, creator, sig, (Instant::now() - t0).as_millis())); }
                    else { logger.log(format!("[WS CREATE] mint={} creator={} sig={}", mint, creator, sig)); }
                    let _ = handle_create_event_fast(rpc_nb.clone(), pump.clone(), mint, creator, logger).await;
                    seen_ws_set.insert(sig);
                    seen_ws.push_back(sig);
                    if seen_ws.len() > max_seen { if let Some(old) = seen_ws.pop_front() { seen_ws_set.remove(&old); } }
                } else {
                    // Debug opcional si no logra parsear (ver formato real)
                    if env_bool("WS_DEBUG_RAW", false) {
                        let preview = txt.chars().take(300).collect::<String>();
                        logger.log(format!("[WS RAW] {}...", preview));
                    }
                }
            }
            WsMessage::Binary(_) => {}
            WsMessage::Ping(p) => { let _ = ws.send(WsMessage::Pong(p)).await; }
            WsMessage::Pong(_) => {}
            WsMessage::Close(_) => { break; }
            WsMessage::Frame(_) => {}
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

    let disc_create = anchor_disc("create");

    let mut seen: VecDeque<Signature> = VecDeque::new();
    let mut seen_set: HashSet<Signature> = HashSet::new();
    let max_seen = env_usize("POLL_SEEN_MAX", 20_000);

    let poll_interval = env_duration_ms("PUMP_POLL_INTERVAL_MS", 150);
    let poll_limit = env_usize("PUMP_POLL_LIMIT", 1000);
    let latency_log = env_bool("LATENCY_LOG", true);

    logger.log("🛰️ run_monitor_loop: escuchando transacciones Pump.fun (fallback)".to_string());

    loop {
        let t_fetch_0 = Instant::now();
        let sigs = rpc
            .get_signatures_for_address_with_config(
                &pump_program,
                GetConfirmedSignaturesForAddress2Config { limit: Some(poll_limit as usize), ..Default::default() },
            )
            .await
            .context("get_signatures_for_address(PUMP_PROGRAM)")?;
        let t_fetch_1 = Instant::now();

        if latency_log { logger.log(format!("[POLL] fetched={} in {}ms", sigs.len(), (t_fetch_1 - t_fetch_0).as_millis())); }

        for s in sigs.into_iter().rev() {
            let Ok(sig) = s.signature.parse::<Signature>() else { continue };
            if seen_set.contains(&sig) { continue; }

            let t0 = Instant::now();
            let Ok(tx) = rpc
                .get_transaction_with_config(&sig, RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    commitment: Some(CommitmentConfig::processed()),
                    max_supported_transaction_version: None,
                })
                .await else { continue };
            let t1 = Instant::now();

            let inner = tx.transaction;
            let meta_opt = inner.meta.clone();
            let enc_tx = inner.transaction;

            if let Some(meta) = meta_opt.clone() {
                match meta.log_messages {
                    OptionSerializer::Some(ref logs) if !contains_create(logs) => {
                        seen_set.insert(sig);
                        seen.push_back(sig);
                        if seen.len() > max_seen { if let Some(old) = seen.pop_front() { seen_set.remove(&old); } }
                        continue;
                    }
                    _ => {}
                }
            }

            match &enc_tx {
                EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                    UiMessage::Raw(raw) => {
                        let mut is_create = false;
                        let mut found_mint: Option<Pubkey> = None;
                        let creator = raw.account_keys.first().and_then(|s| s.parse::<Pubkey>().ok());
                        for ix in &raw.instructions {
                            let Some(prog_id) = raw.account_keys.get(ix.program_id_index as usize) else { continue };
                            if prog_id != &PUMP_PROGRAM { continue; }
                            if let Ok(bytes) = bs58::decode(&ix.data).into_vec() {
                                if bytes.len() >= 8 && bytes[..8] == disc_create {
                                    is_create = true;
                                    if let Some(&acc0) = ix.accounts.first() {
                                        found_mint = raw.account_keys.get(acc0 as usize).and_then(|s| s.parse::<Pubkey>().ok());
                                    }
                                    break;
                                }
                            }
                        }
                        if is_create {
                            if let (Some(mint), Some(creator_pk)) = (found_mint, creator) {
                                if latency_log { logger.log(format!("[CREATE/raw] mint={} creator={} sig={} | rpc={}ms", mint, creator_pk, sig, (t1 - t0).as_millis())); }
                                else { logger.log(format!("[CREATE/raw] mint={} creator={}", mint, creator_pk)); }
                                let t2 = Instant::now();
                                let _ = handle_create_event_fast(rpc.clone(), pump.clone(), mint, creator_pk, logger).await;
                                if latency_log { let t3 = Instant::now(); logger.log(format!("[CREATE DONE] sig={} | handle={}ms total={}ms", sig, (t3 - t2).as_millis(), (t3 - t0).as_millis())); }
                            }
                        }
                    }
                    UiMessage::Parsed(parsed) => {
                        if let (Some(creator_pk), Some(meta)) = (parsed.account_keys.first().and_then(|k| k.pubkey.parse().ok()), meta_opt) {
                            match meta.log_messages {
                                OptionSerializer::Some(logs) => {
                                    if logs.iter().any(|l| l.contains("Instruction: Create")) {
                                        let mint = parsed.account_keys.get(0).and_then(|k| k.pubkey.parse().ok());
                                        if let Some(mint) = mint {
                                            if latency_log { logger.log(format!("[CREATE/logs] mint={} creator={} sig={} | rpc={}ms", mint, creator_pk, sig, (t1 - t0).as_millis())); }
                                            else { logger.log(format!("[CREATE/logs] mint={} creator={}", mint, creator_pk)); }
                                            let t2 = Instant::now();
                                            let _ = handle_create_event_fast(rpc.clone(), pump.clone(), mint, creator_pk, logger).await;
                                            if latency_log { let t3 = Instant::now(); logger.log(format!("[CREATE DONE] sig={} | handle={}ms total={}ms", sig, (t3 - t2).as_millis(), (t3 - t0).as_millis())); }
                                        }
                                    }
                                }
                                _ => {}
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

        sleep(poll_interval).await;
    }
}
