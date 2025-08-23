use std::sync::Arc;

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;

use crate::{
    common::logger::Logger,
    dex::pump_fun::{Pump, SwapConfig},
    engine::{
        monitor::creator_checks::fetch_creator_stats,
        swap::SwapDirection,
    },
};

#[derive(Clone, Debug)]
struct SniperPolicy {
    /// Máximo de creaciones anteriores permitidas (excluye el mint actual)
    max_prev_creations: usize,            // ej. 0
    /// Máximo SOL (lamports) comprados por el creador en el mint actual para permitir entrada
    max_creator_self_buy_lamports: u64,   // ej. 2 SOL = 2_000_000_000
    /// Cuántas firmas recientes del creator escanear
    creator_scan_limit: usize,            // ej. 120
    /// Slippage en bps para el BUY
    slippage_bps: u64,                    // ej. 500 = 5%
}

impl SniperPolicy {
    fn from_env() -> Self {
        let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let to_usize = |s: String, d: usize| s.parse().unwrap_or(d);
        let to_u64 = |s: String, d: u64| s.parse().unwrap_or(d);
        Self {
            max_prev_creations: to_usize(get("CREATOR_LIMIT", "0"), 0),
            max_creator_self_buy_lamports: to_u64(
                get("CREATOR_MAX_SELF_BUY_SOL_LAMPORTS", "2000000000"), // 2 SOL
                2_000_000_000,
            ),
            creator_scan_limit: to_usize(get("CREATOR_SCAN_LIMIT", "120"), 120),
            slippage_bps: to_u64(get("BUY_SLIPPAGE_BPS", "500"), 500),
        }
    }
}

/// Llama esto cuando detectes un `CreateEvent` de pump.fun.
pub async fn handle_create_event_fast(
    rpc_nb: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pump: Arc<Pump>,
    mint: Pubkey,
    creator: Pubkey,
    logger: &Logger,
) -> Result<()> {
    let policy = SniperPolicy::from_env();

    // (Opcional) un pequeño backoff ayuda a capturar la primera buy del creador si cae en el bloque siguiente
    // tokio::time::sleep(Duration::from_millis(200)).await;

    let stats = fetch_creator_stats(
        rpc_nb.clone(),
        creator,
        mint,
        policy.creator_scan_limit,
    ).await?;

    // `created_mints` incluye el actual; cuenta sólo previos.
    let prev_creations = stats.created_mints.iter().filter(|m| **m != mint).count();

    let pass = (prev_creations <= policy.max_prev_creations)
        && (stats.total_buys_lamports <= policy.max_creator_self_buy_lamports);

    logger.log(format!(
        "[CREATE {:?}] prev_creations={} self_buy_lamports={} pass={}",
        mint, prev_creations, stats.total_buys_lamports, pass
    ));

    if !pass {
        return Ok(());
    }

    // === Ejecutar BUY ===
    // NOTA: ahora mismo `Pump::swap` compra por `amount` de tokens (base units).
    // Puedes cambiar a “presupuesto en SOL” más adelante con una búsqueda binaria usando tus quotes.

    let buy_amount_tokens: u64 = std::env::var("BUY_AMOUNT_TOKENS_RAW")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000); // ajusta según el token

    let cfg = SwapConfig {
        amount: buy_amount_tokens,
        slippage_bps: policy.slippage_bps,
        use_zeroslot: true,
        swap_direction: SwapDirection::Buy,
    };

    // Dispara sin bloquear el hilo del monitor.
    let pump2 = pump.clone();
    tokio::spawn(async move {
        match pump2.swap(&mint.to_string(), cfg).await {
            Ok(sigs) => eprintln!("[BUY {:?}] OK sigs={:?}", mint, sigs),
            Err(e) => eprintln!("[BUY {:?}] ERR: {:?}", mint, e),
        }
    });

    Ok(())
}
