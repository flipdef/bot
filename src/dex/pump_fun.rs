// src/dex/pump_fun.rs — versión limpia (sin duplicados)
// Mantiene el layout y las cuentas exactamente como tu IDL.

use std::{str::FromStr, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use borsh::from_slice;
use borsh_derive::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program,
    sysvar::rent,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token; // ID del programa token

use crate::{
    common::logger::Logger,
    core::tx,
    engine::swap::SwapDirection,
};

// Si ya tienes SwapConfig en otro módulo, elimina esta definición local y corrige el `use`.
#[derive(Clone, Copy, Debug)]
pub struct SwapConfig {
    pub amount: u64,
    pub slippage_bps: u64,
    pub use_zeroslot: bool,
    pub swap_direction: SwapDirection,
}

pub const TEN_THOUSAND: u64 = 10_000;
pub const PUMP_GLOBAL: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
pub const PUMP_FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
pub const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const PUMP_BUY_METHOD: u64 = 16_927_863_322_537_952_870; // 8-byte discriminator
pub const PUMP_SELL_METHOD: u64 = 12_502_976_635_542_562_355; // 8-byte discriminator

pub struct Pump {
    pub rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pub keypair: Arc<Keypair>,
    pub rpc_client: Option<Arc<solana_client::rpc_client::RpcClient>>,
}

impl Pump {
    pub fn new(
        rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
        rpc_client: Arc<solana_client::rpc_client::RpcClient>,
        keypair: Arc<Keypair>,
    ) -> Self {
        Self { rpc_nonblocking_client, keypair, rpc_client: Some(rpc_client) }
    }

    /// Construye y envía `buy`/`sell` según tu IDL. Usa `SwapConfig::{amount, slippage_bps, use_zeroslot, swap_direction}`.
    pub async fn swap(&self, mint: &str, swap_config: SwapConfig) -> Result<Vec<String>> {
        let logger = Logger::new("[SWAP PUMP] => ".to_string());
        let owner = self.keypair.pubkey();
        let mint = Pubkey::from_str(mint).map_err(|e| anyhow!("failed to parse mint pubkey: {}", e))?;

        let pump_program = Pubkey::from_str(PUMP_PROGRAM).context("parsing pump program id")?;
        let global = Pubkey::from_str(PUMP_GLOBAL).context("parsing global addr")?;
        let fee_recipient = Pubkey::from_str(PUMP_FEE_RECIPIENT).context("parsing fee recipient")?;
        let event_authority = get_event_authority_pda(&pump_program);
        let rent_sysvar = rent::id();

        let client = self.rpc_client.as_ref().context("rpc_client no seteado")?;

        // Derivar PDAs/ATAs requeridas por IDL
        let (bonding_curve, associated_bonding_curve, bc_account) =
            get_bonding_curve_account(client.clone(), &mint, &pump_program).await?;
        let associated_user = get_associated_token_address(&owner, &mint);

        // Asegurar ATA del usuario (el programa espera que exista)
        let mut ixs: Vec<Instruction> = Vec::new();
        if client.get_account(&associated_user).is_err() {
            ixs.push(create_associated_token_account(&owner, &owner, &mint, &spl_token::ID));
        }

        // Leer Global para conocer fee_bps y cotizar límites de slippage
        let global_acc = get_global_account(client.clone(), &global)?;
        let amount = swap_config.amount; // tokens
        let slippage_bps = swap_config.slippage_bps;

        let pump_ix = match swap_config.swap_direction {
            SwapDirection::Buy => {
                let sol_cost = quote_buy_cost(
                    bc_account.virtual_token_reserves,
                    bc_account.virtual_sol_reserves,
                    amount,
                    global_acc.fee_basis_points,
                )?;
                let max_sol_cost = max_amount_with_slippage(sol_cost, slippage_bps);
                build_buy_ix(
                    pump_program,
                    global,
                    fee_recipient,
                    &mint,
                    bonding_curve,
                    associated_bonding_curve,
                    associated_user,
                    owner,
                    rent_sysvar,
                    event_authority,
                    amount,
                    max_sol_cost,
                )
            }
            SwapDirection::Sell => {
                let sol_out = quote_sell_out(
                    bc_account.virtual_token_reserves,
                    bc_account.virtual_sol_reserves,
                    amount,
                    global_acc.fee_basis_points,
                )?;
                let min_sol_output = min_amount_with_slippage(sol_out, slippage_bps);
                build_sell_ix(
                    pump_program,
                    global,
                    fee_recipient,
                    &mint,
                    bonding_curve,
                    associated_bonding_curve,
                    associated_user,
                    owner,
                    event_authority,
                    amount,
                    min_sol_output,
                )
            }
        };

        ixs.push(pump_ix);

        let sigs = tx::new_signed_and_send(
            client,
            &self.keypair,
            ixs,
            swap_config.use_zeroslot,
            &logger,
        )
        .await?;

        Ok(sigs)
    }
}

fn min_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .saturating_mul(TEN_THOUSAND.saturating_sub(slippage_bps))
        .saturating_div(TEN_THOUSAND)
}

fn max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .saturating_mul(TEN_THOUSAND.saturating_add(slippage_bps))
        .saturating_div(TEN_THOUSAND)
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaydiumInfo {
    pub base: f64,
    pub quote: f64,
    pub price: f64,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PumpInfo {
    pub mint: String,
    pub bonding_curve: String,
    pub associated_bonding_curve: String,
    pub raydium_pool: Option<String>,
    pub raydium_info: Option<RaydiumInfo>,
    pub complete: bool,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub total_supply: u64,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
pub struct BondingCurveAccount {
    pub discriminator: u64,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
}

#[derive(Debug, BorshSerialize, BorshDeserialize, Clone, Copy)]
pub struct GlobalAccount {
    pub discriminator: u64,
    pub initialized: bool,
    pub authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub initial_virtual_token_reserves: u64,
    pub initial_virtual_sol_reserves: u64,
    pub initial_real_token_reserves: u64,
    pub token_total_supply: u64,
    pub fee_basis_points: u64,
}

pub async fn get_bonding_curve_account(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<(Pubkey, Pubkey, BondingCurveAccount)> {
    let bonding_curve = get_pda(mint, program_id)?;
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, mint);
    let bonding_curve_data = rpc_client.get_account_data(&bonding_curve)?;

    let bonding_curve_account = from_slice::<BondingCurveAccount>(&bonding_curve_data)
        .map_err(|e| anyhow!("Failed to deserialize bonding curve account: {}", e))?;

    Ok((bonding_curve, associated_bonding_curve, bonding_curve_account))
}

pub fn get_pda(mint: &Pubkey, program_id: &Pubkey) -> Result<Pubkey> {
    let seeds = [b"bonding-curve".as_ref(), mint.as_ref()];
    let (bonding_curve, _bump) = Pubkey::find_program_address(&seeds, program_id);
    Ok(bonding_curve)
}

pub async fn get_pump_info(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: &str,
) -> Result<PumpInfo> {
    let mint = Pubkey::from_str(mint)?;
    let program_id = Pubkey::from_str(PUMP_PROGRAM)?;
    let (bonding_curve, associated_bonding_curve, bonding_curve_account) =
        get_bonding_curve_account(rpc_client, &mint, &program_id).await?;

    let pump_info = PumpInfo {
        mint: mint.to_string(),
        bonding_curve: bonding_curve.to_string(),
        associated_bonding_curve: associated_bonding_curve.to_string(),
        raydium_pool: None,
        raydium_info: None,
        complete: bonding_curve_account.complete,
        virtual_sol_reserves: bonding_curve_account.virtual_sol_reserves,
        virtual_token_reserves: bonding_curve_account.virtual_token_reserves,
        total_supply: bonding_curve_account.token_total_supply,
    };
    Ok(pump_info)
}

fn get_event_authority_pda(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program_id).0
}

fn get_global_account(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    global: &Pubkey,
) -> Result<GlobalAccount> {
    let data = rpc_client
        .get_account_data(global)
        .context("cargando Global account")?;
    let acc: GlobalAccount = from_slice(&data).context("deserializando Global (borsh)")?;
    if !acc.initialized {
        bail!("Global no inicializado");
    }
    Ok(acc)
}

fn build_buy_ix(
    pump_program: Pubkey,
    global: Pubkey,
    fee_recipient: Pubkey,
    mint: &Pubkey,
    bonding_curve: Pubkey,
    associated_bonding_curve: Pubkey,
    associated_user: Pubkey,
    user: Pubkey,
    _rent_sysvar: Pubkey,
    event_authority: Pubkey,
    amount: u64,
    max_sol_cost: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8);
    data.extend_from_slice(&PUMP_BUY_METHOD.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    Instruction {
        program_id: pump_program,
        accounts: vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(fee_recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(bonding_curve, false),
            AccountMeta::new(associated_bonding_curve, false),
            AccountMeta::new(associated_user, false),
            AccountMeta::new(user, true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(rent::id(), false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(pump_program, false), // `program` en IDL
        ],
        data,
    }
}

fn build_sell_ix(
    pump_program: Pubkey,
    global: Pubkey,
    fee_recipient: Pubkey,
    mint: &Pubkey,
    bonding_curve: Pubkey,
    associated_bonding_curve: Pubkey,
    associated_user: Pubkey,
    user: Pubkey,
    event_authority: Pubkey,
    amount: u64,
    min_sol_output: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8);
    data.extend_from_slice(&PUMP_SELL_METHOD.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    Instruction {
        program_id: pump_program,
        accounts: vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(fee_recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(bonding_curve, false),
            AccountMeta::new(associated_bonding_curve, false),
            AccountMeta::new(associated_user, false),
            AccountMeta::new(user, true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_associated_token_account::ID, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(pump_program, false), // `program` en IDL
        ],
        data,
    }
}

// === Cotización de curva CP con fee ===
fn quote_buy_cost(vt: u64, vs: u64, amount: u64, fee_bps: u64) -> Result<u64> {
    if amount == 0 { return Ok(0); }
    let vt = vt as u128;
    let vs = vs as u128;
    let amount = amount as u128;
    if amount >= vt { bail!("amount >= virtual_token_reserves"); }
    let k = vt.checked_mul(vs).context("overflow k")?;
    let vt_new = vt - amount;
    let vs_new = k / vt_new;
    let delta = vs_new.checked_sub(vs).unwrap();
    let fee = delta * (fee_bps as u128) / (TEN_THOUSAND as u128);
    Ok((delta + fee) as u64)
}

fn quote_sell_out(vt: u64, vs: u64, amount: u64, fee_bps: u64) -> Result<u64> {
    if amount == 0 { return Ok(0); }
    let vt = vt as u128;
    let vs = vs as u128;
    let amount = amount as u128;
    let k = vt.checked_mul(vs).context("overflow k")?;
    let vt_new = vt + amount;
    let vs_new = k / vt_new;
    let delta = vs - vs_new;
    let out = delta * ((TEN_THOUSAND - fee_bps) as u128) / (TEN_THOUSAND as u128);
    Ok(out as u64)
}
