// Monitor Helius + caché LRU con TTL para decisiones de snipe por creador

use std::{
    collections::{HashMap, VecDeque},
    env,
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient as NbRpcClient;
use solana_sdk::pubkey::Pubkey;

use super::creator_checks::{fetch_creator_stats, should_snipe, CreatorStats};

// === NUEVO: imports para disparar autosnipe desde aquí (opcional) ===
use crate::{
    common::logger::Logger,
    dex::pump_fun::Pump,
    engine::monitor::auto_snipe::handle_create_event_fast,
};

// === Utilidades ENV ===
fn env_usize(key: &str, default: usize) -> usize {
    env::var(key).ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(default)
}
fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
}
fn env_duration_ms(key: &str, default_ms: u64) -> Duration {
    Duration::from_millis(env_u64(key, default_ms))
}

// === LRU TTL Cache (simple, sin deps) ===
struct LruTtlCache<K, V> {
    map: HashMap<K, (V, Instant)>, // (valor, última vez de acceso)
    order: VecDeque<K>,            // más antiguo al frente
    cap: usize,
    ttl: Duration,
}

impl<K: Eq + Hash + Clone, V: Clone> LruTtlCache<K, V> {
    fn new(cap: usize, ttl: Duration) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), cap, ttl }
    }

    fn touch(&mut self, k: &K) {
        // por simplicidad O(n)
        self.order.retain(|x| x != k);
        self.order.push_back(k.clone());
        if let Some((_, t)) = self.map.get_mut(k) {
            *t = Instant::now();
        }
    }

    fn remove(&mut self, k: &K) {
        self.map.remove(k);
        self.order.retain(|x| x != k);
    }

    fn insert(&mut self, k: K, v: V) {
        if self.map.contains_key(&k) {
            if let Some((val, t)) = self.map.get_mut(&k) {
                *val = v;
                *t = Instant::now();
            }
            self.touch(&k);
            return;
        }
        if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(k.clone());
        self.map.insert(k, (v, Instant::now()));
    }

    /// Devuelve un **clon** del valor (evita E0502 al mutar self).
    fn get(&mut self, k: &K) -> Option<V> {
        let now = Instant::now();
        let within = match self.map.get(k) {
            Some((_, t)) => now.duration_since(*t) <= self.ttl,
            None => return None,
        };
        if within {
            let val = self.map.get(k).map(|(v, _)| v.clone());
            self.touch(k);
            val
        } else {
            self.remove(k);
            None
        }
    }
}

// === Monitor ===
pub struct HeliusMonitor {
    rpc: Arc<NbRpcClient>,
    cache: LruTtlCache<Pubkey, CreatorStats>,
    // umbrales
    max_prev_creations: usize,
    min_creator_buy_lamports: u64,
    scan_limit: usize,
}

impl HeliusMonitor {
    pub fn new(rpc: Arc<NbRpcClient>) -> Self {
        let cap = env_usize("CREATOR_STATS_CACHE_CAP", 1024);
        let ttl = env_duration_ms("CREATOR_STATS_CACHE_TTL_MS", 45_000);
        let max_prev_creations = env_usize("CREATOR_MAX_PREV_CREATIONS", 3);
        let min_creator_buy_lamports = env_u64("CREATOR_MIN_BUY_SOL_LAMPORTS", 50_000_000); // ~0.05 SOL
        let scan_limit = env_usize("CREATOR_SCAN_LIMIT", 200);

        Self {
            rpc,
            cache: LruTtlCache::new(cap, ttl),
            max_prev_creations,
            min_creator_buy_lamports,
            scan_limit,
        }
    }

    /// Llamar cuando detectes un `CreateEvent` en pump.fun — retorna stats y decisión
    pub async fn on_pump_create_event(
        &mut self,
        creator: Pubkey,
        mint: Pubkey,
    ) -> Result<(CreatorStats, bool)> {
        if let Some(stats) = self.cache.get(&creator) {
            let go = should_snipe(&stats, self.max_prev_creations, self.min_creator_buy_lamports);
            return Ok((stats, go));
        }

        let stats = fetch_creator_stats(Arc::clone(&self.rpc), creator, mint, self.scan_limit)
            .await
            .with_context(|| format!("fetch_creator_stats for creator={creator} mint={mint}"))?;

        self.cache.insert(creator, stats.clone());
        let go = should_snipe(&stats, self.max_prev_creations, self.min_creator_buy_lamports);
        Ok((stats, go))
    }
}

/// Helper: dispara el autosnipe asincrónico (no deja caer el loop del monitor)
pub async fn on_pumpfun_create_event(
    rpc_nb: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pump: Arc<Pump>,
    mint: Pubkey,
    creator: Pubkey,
    logger: &Logger,
) {
    let _ = handle_create_event_fast(rpc_nb, pump, mint, creator, logger).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_ttl_get_returns_clone_and_updates_recency() {
        let mut c: LruTtlCache<u8, String> = LruTtlCache::new(2, Duration::from_millis(10_000));
        c.insert(1, "a".into());
        let v1 = c.get(&1).unwrap();
        assert_eq!(v1, "a");
        c.insert(2, "b".into());
        c.insert(3, "c".into()); // expulsa 1 o 2 según recencia
    }
}
