use serde::{Deserialize, Serialize};

/// Dirección del swap (compra o venta)
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SwapDirection {
    Buy,
    Sell,
}

/// Tipo de entrada para el swap
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SwapInType {
    /// Cantidad exacta de tokens a entregar (ej: 1 SOL exacto)
    ExactIn,
    /// Cantidad exacta de tokens a recibir (ej: quiero recibir 1000 tokens exactos)
    ExactOut,
}

/// Helpers de slippage
pub const TEN_THOUSAND: u64 = 10_000;

/// Devuelve el mínimo permitido con slippage (para compras: recibes al menos X)
pub fn min_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .checked_mul(TEN_THOUSAND.saturating_sub(slippage_bps))
        .unwrap_or(0)
        .checked_div(TEN_THOUSAND)
        .unwrap_or(0)
}

/// Devuelve el máximo permitido con slippage (para ventas: no gastas más de X)
pub fn max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .checked_mul(slippage_bps.saturating_add(TEN_THOUSAND))
        .unwrap_or(0)
        .checked_div(TEN_THOUSAND)
        .unwrap_or(0)
}

/// Descripción en string para debug/logs
pub fn describe_swap(direction: SwapDirection, in_type: SwapInType) -> &'static str {
    match (direction, in_type) {
        (SwapDirection::Buy, SwapInType::ExactIn) => "Buy with Exact In",
        (SwapDirection::Buy, SwapInType::ExactOut) => "Buy aiming Exact Out",
        (SwapDirection::Sell, SwapInType::ExactIn) => "Sell with Exact In",
        (SwapDirection::Sell, SwapInType::ExactOut) => "Sell aiming Exact Out",
    }
}
