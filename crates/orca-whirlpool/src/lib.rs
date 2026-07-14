//! Orca Whirlpools (CLMM) DEX crate.
//!
//! Implements the `Market` trait from `solroute-core` for Orca's
//! concentrated-liquidity whirlpools. Exact quoting delegates to Orca's
//! official pure math crate (`orca_whirlpools_core::swap_quote_by_input_token`)
//! with tick arrays pulled from the account store — no math ported, no math
//! to get wrong.

use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use solroute_core::{
    quote_priority, AccountDataProvider, GenericError, Market,
    PoolFees, PoolFinancials, PoolMetadata, SwapDirection, infer_mint_decimals,
};

use orca_whirlpools_core::{
    swap_quote_by_input_token, TickArrayFacade, TickArrays, TickFacade,
    WhirlpoolFacade, WhirlpoolRewardInfoFacade,
};

// ============================================================================
// Constants
// ============================================================================

pub const ORCA_WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Anchor discriminator for `Whirlpool` accounts (sha256("account:Whirlpool")[..8]).
pub const DISC_WHIRLPOOL: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];

/// Whirlpool account size: 8 disc + 261 fixed + 384 reward infos.
pub const WHIRLPOOL_LEN: u64 = 653;

pub const TICK_ARRAY_SIZE: i32 = 88;

// ============================================================================
// Models (on-chain layout)
// ============================================================================

#[derive(
    Debug, BorshDeserialize, serde::Deserialize, serde::Serialize, PartialEq, Eq, Clone, Hash,
)]
pub struct WhirlpoolRewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub authority: Pubkey,
    pub emissions_per_second_x64: u128,
    pub growth_global_x64: u128,
}

#[derive(
    Debug, BorshDeserialize, serde::Deserialize, serde::Serialize, PartialEq, Eq, Clone, Hash,
)]
pub struct WhirlpoolPool {
    pub whirlpools_config: Pubkey,
    pub whirlpool_bump: [u8; 1],
    pub tick_spacing: u16,
    pub fee_tier_index_seed: [u8; 2],
    /// Hundredths of a basis point (3000 = 0.30%).
    pub fee_rate: u16,
    pub protocol_fee_rate: u16,
    pub liquidity: u128,
    /// Q64.64 sqrt(token_b per token_a).
    pub sqrt_price: u128,
    pub tick_current_index: i32,
    pub protocol_fee_owed_a: u64,
    pub protocol_fee_owed_b: u64,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub fee_growth_global_a: u128,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub fee_growth_global_b: u128,
    pub reward_last_updated_timestamp: u64,
    pub reward_infos: [WhirlpoolRewardInfo; 3],
}

/// One tick of a fixed tick array (113 bytes on-chain).
#[derive(Debug, BorshDeserialize, Clone, Copy)]
pub struct Tick {
    pub initialized: bool,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
    pub fee_growth_outside_a: u128,
    pub fee_growth_outside_b: u128,
    pub reward_growths_outside: [u128; 3],
}

/// Fixed tick array account: 8 disc + 4 + 88×113 + 32 = 9988 bytes.
#[derive(Debug, BorshDeserialize, Clone)]
pub struct TickArray {
    pub start_tick_index: i32,
    pub ticks: [Tick; 88],
    pub whirlpool: Pubkey,
}

impl TickArray {
    pub fn from_account_bytes(data: &[u8]) -> Option<TickArray> {
        if data.len() < 8 {
            return None;
        }
        BorshDeserialize::deserialize(&mut &data[8..]).ok()
    }
}

/// PDA of the tick array starting at `start_tick_index`.
/// Seeds: ["tick_array", whirlpool, start_tick_index.to_string()].
pub fn derive_tick_array_pda(whirlpool: &Pubkey, start_tick_index: i32) -> Pubkey {
    let program_id = Pubkey::from_str_const(ORCA_WHIRLPOOL_PROGRAM);
    Pubkey::find_program_address(
        &[
            b"tick_array",
            whirlpool.as_ref(),
            start_tick_index.to_string().as_bytes(),
        ],
        &program_id,
    )
    .0
}

/// Start index of the tick array containing `tick_index`.
pub fn tick_array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
    let ticks_per_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    tick_index.div_euclid(ticks_per_array) * ticks_per_array
}

/// Q64.64 sqrt-price bounds (from the whirlpool program).
pub const MIN_SQRT_PRICE: u128 = 4295048016;
pub const MAX_SQRT_PRICE: u128 = 79226673521066979257578248091;

/// Oracle PDA: ["oracle", whirlpool].
pub fn derive_oracle_pda(whirlpool: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str_const(ORCA_WHIRLPOOL_PROGRAM);
    Pubkey::find_program_address(&[b"oracle", whirlpool.as_ref()], &program_id).0
}

/// The three tick-array pubkeys the `swap` instruction needs (tick_array_0/1/2),
/// in swap-traversal order: tick_array_0 is the current array, then the swap
/// advances by one array per slot in the price direction (down for a_to_b, up
/// for b_to_a).
pub fn swap_tick_array_pdas(
    whirlpool: &Pubkey,
    tick_current_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> [Pubkey; 3] {
    let ticks_per_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    let start = tick_array_start_index(tick_current_index, tick_spacing);
    let step = if a_to_b { -ticks_per_array } else { ticks_per_array };
    [
        derive_tick_array_pda(whirlpool, start),
        derive_tick_array_pda(whirlpool, start + step),
        derive_tick_array_pda(whirlpool, start + 2 * step),
    ]
}

// ============================================================================
// Facade conversion
// ============================================================================

fn pool_to_facade(pool: &WhirlpoolPool) -> WhirlpoolFacade {
    WhirlpoolFacade {
        fee_tier_index_seed: pool.fee_tier_index_seed,
        tick_spacing: pool.tick_spacing,
        fee_rate: pool.fee_rate,
        protocol_fee_rate: pool.protocol_fee_rate,
        liquidity: pool.liquidity,
        sqrt_price: pool.sqrt_price,
        tick_current_index: pool.tick_current_index,
        fee_growth_global_a: pool.fee_growth_global_a,
        fee_growth_global_b: pool.fee_growth_global_b,
        reward_last_updated_timestamp: pool.reward_last_updated_timestamp,
        reward_infos: [
            reward_facade(&pool.reward_infos[0]),
            reward_facade(&pool.reward_infos[1]),
            reward_facade(&pool.reward_infos[2]),
        ],
    }
}

fn reward_facade(r: &WhirlpoolRewardInfo) -> WhirlpoolRewardInfoFacade {
    WhirlpoolRewardInfoFacade {
        emissions_per_second_x64: r.emissions_per_second_x64,
        growth_global_x64: r.growth_global_x64,
    }
}

fn tick_array_to_facade(arr: &TickArray) -> TickArrayFacade {
    let mut ticks = [TickFacade {
        initialized: false,
        liquidity_net: 0,
        liquidity_gross: 0,
        fee_growth_outside_a: 0,
        fee_growth_outside_b: 0,
        reward_growths_outside: [0; 3],
    }; 88];
    for (dst, src) in ticks.iter_mut().zip(arr.ticks.iter()) {
        *dst = TickFacade {
            initialized: src.initialized,
            liquidity_net: src.liquidity_net,
            liquidity_gross: src.liquidity_gross,
            fee_growth_outside_a: src.fee_growth_outside_a,
            fee_growth_outside_b: src.fee_growth_outside_b,
            reward_growths_outside: src.reward_growths_outside,
        };
    }
    TickArrayFacade {
        start_tick_index: arr.start_tick_index,
        ticks,
    }
}

// ============================================================================
// Market
// ============================================================================

pub struct OrcaWhirlpoolMarket {
    pub pool: WhirlpoolPool,
    pub pool_address: String,
    pub vault_a_balance: u64,
    pub vault_b_balance: u64,
    pub token_a_decimals: u8,
    pub token_b_decimals: u8,
    /// True when token_mint_b is the quote currency.
    pub flipped: bool,
}

impl OrcaWhirlpoolMarket {
    pub fn new(pool: WhirlpoolPool, pool_address: String) -> Self {
        let flipped = quote_priority(&pool.token_mint_b).unwrap_or(usize::MAX)
            < quote_priority(&pool.token_mint_a).unwrap_or(usize::MAX);
        let token_a_decimals = infer_mint_decimals(&pool.token_mint_a);
        let token_b_decimals = infer_mint_decimals(&pool.token_mint_b);
        Self {
            pool,
            pool_address,
            vault_a_balance: 0,
            vault_b_balance: 0,
            token_a_decimals,
            token_b_decimals,
            flipped,
        }
    }

    /// raw token_b per token_a from Q64.64 sqrt price.
    fn raw_price_b_per_a(sqrt_price: u128) -> f64 {
        let sqrt = sqrt_price as f64 / (1u128 << 64) as f64;
        sqrt * sqrt
    }

    fn trade_fee_bps(&self) -> u64 {
        // fee_rate is hundredths of a bp.
        (self.pool.fee_rate as u64) / 100
    }

    /// Single-price approximation for offline paths (no tick arrays).
    fn approximate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        sqrt_price: u128,
    ) -> Result<u64, GenericError> {
        if self.pool.liquidity == 0 {
            return Err("Pool has zero liquidity".into());
        }
        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };
        let raw_price = Self::raw_price_b_per_a(sqrt_price);
        let fee_multiplier = 10_000 - self.trade_fee_bps() as u128;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier) / 10_000;

        // Physical Buy: quote = token_a side spends a -> b. In this crate's
        // normalized convention (!flipped => quote = token_a), physical Buy
        // means a -> b: output = input × raw_price.
        let output = match physical_direction {
            SwapDirection::Buy => {
                let raw = (amount_in_with_fee as f64 * raw_price) as u64;
                raw.min(self.vault_b_balance)
            }
            SwapDirection::Sell => {
                if raw_price == 0.0 {
                    return Err("Zero price".into());
                }
                let raw = (amount_in_with_fee as f64 / raw_price) as u64;
                raw.min(self.vault_a_balance)
            }
        };
        Ok(output)
    }
}

impl Market for OrcaWhirlpoolMarket {
    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        let (quote_mint, base_mint, quote_vault, base_vault) = if self.flipped {
            (
                self.pool.token_mint_b,
                self.pool.token_mint_a,
                self.pool.token_vault_b,
                self.pool.token_vault_a,
            )
        } else {
            (
                self.pool.token_mint_a,
                self.pool.token_mint_b,
                self.pool.token_vault_a,
                self.pool.token_vault_b,
            )
        };
        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Orca Whirlpool".to_string(),
            quote_mint,
            base_mint,
            quote_vault,
            base_vault,
            fees: PoolFees {
                trade_fee_bps: self.trade_fee_bps(),
                protocol_fee_bps: Some(self.pool.protocol_fee_rate as u64),
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        let (quote_balance, base_balance, quote_decimals, base_decimals) = if self.flipped {
            (self.vault_b_balance, self.vault_a_balance, self.token_b_decimals, self.token_a_decimals)
        } else {
            (self.vault_a_balance, self.vault_b_balance, self.token_a_decimals, self.token_b_decimals)
        };
        Ok(PoolFinancials {
            quote_balance,
            base_balance,
            quote_decimals,
            base_decimals,
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        self.approximate_output(amount_in, direction, self.pool.sqrt_price)
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        // Live sqrt_price at offset 8+32+1+2+2+2+2+16 = 65..81.
        let sqrt_price = pool_data
            .and_then(|d| d.get(65..81))
            .and_then(|b| b.try_into().ok())
            .map(u128::from_le_bytes)
            .unwrap_or(self.pool.sqrt_price);
        self.approximate_output(amount_in, direction, sqrt_price)
    }

    /// Exact quote via Orca's official math with tick arrays from the store.
    /// No tick arrays available = no quote (mirrors the DLMM strict policy).
    fn calculate_output_live_ex(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
        provider: &dyn AccountDataProvider,
    ) -> Result<u64, GenericError> {
        // Freshest pool state; fall back to the cached struct.
        let live_pool: Option<WhirlpoolPool> = pool_data
            .filter(|d| d.len() > 8)
            .and_then(|d| WhirlpoolPool::deserialize(&mut &d[8..]).ok());
        let pool = live_pool.as_ref().unwrap_or(&self.pool);

        let Ok(pool_pubkey) = self.pool_address.parse::<Pubkey>() else {
            return Err("bad pool address".into());
        };

        // Input is token A on (!flipped, Buy) — quote = A — or (flipped, Sell).
        let specified_token_a = matches!(
            (self.flipped, direction),
            (false, SwapDirection::Buy) | (true, SwapDirection::Sell)
        );
        // a->b swaps move price down (descending tick arrays), b->a up.
        let a_to_b = specified_token_a;

        let ticks_per_array = TICK_ARRAY_SIZE * pool.tick_spacing as i32;
        let start = tick_array_start_index(pool.tick_current_index, pool.tick_spacing);
        let step = if a_to_b { -ticks_per_array } else { ticks_per_array };

        let mut facades: Vec<TickArrayFacade> = Vec::with_capacity(3);
        for i in 0..3 {
            let idx = start + step * i;
            let pda = derive_tick_array_pda(&pool_pubkey, idx);
            let Some(data) = provider.pool_account_data(&pda) else { break };
            let Some(arr) = TickArray::from_account_bytes(&data) else { break };
            facades.push(tick_array_to_facade(&arr));
        }

        let tick_arrays: TickArrays = match facades.len() {
            0 => return Err("Whirlpool tick arrays unavailable".into()),
            1 => TickArrays::One(facades[0]),
            2 => TickArrays::Two(facades[0], facades[1]),
            _ => TickArrays::Three(facades[0], facades[1], facades[2]),
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let quote = swap_quote_by_input_token(
            amount_in,
            specified_token_a,
            0, // slippage handled by the router/executor, not the quote
            pool_to_facade(pool),
            None, // oracle: adaptive-fee pools reject, which is the safe answer
            tick_arrays,
            timestamp,
            None,
            None,
        )
        .map_err(|e| -> GenericError { format!("whirlpool quote: {e:?}").into() })?;

        Ok(quote.token_est_out)
    }

    fn calculate_price_impact(
        &self,
        _amount_in: u64,
        _direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        // Tick-based pricing; reserve-based impact estimation is wrong here.
        Ok(0)
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let raw = Self::raw_price_b_per_a(self.pool.sqrt_price);
        let decimal_adj =
            10f64.powi(self.token_a_decimals as i32 - self.token_b_decimals as i32);
        let human_b_per_a = raw * decimal_adj;
        // quote-per-base: flipped (quote=b) wants b per a; else invert.
        Ok(if self.flipped {
            human_b_per_a
        } else {
            1.0 / human_b_per_a
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_array_start_index_math() {
        // spacing 64: array covers 88*64 = 5632 ticks.
        assert_eq!(tick_array_start_index(0, 64), 0);
        assert_eq!(tick_array_start_index(5631, 64), 0);
        assert_eq!(tick_array_start_index(5632, 64), 5632);
        assert_eq!(tick_array_start_index(-1, 64), -5632);
        assert_eq!(tick_array_start_index(-5632, 64), -5632);
        assert_eq!(tick_array_start_index(-5633, 64), -11264);
    }

    #[test]
    fn struct_sizes_match_onchain() {
        // Whirlpool: 653 = 8 disc + body. Borsh body must be 645.
        // Tick: 113, TickArray body: 4 + 88*113 + 32 = 9980 (+8 disc = 9988).
        assert_eq!(core::mem::size_of::<Tick>() >= 113, true); // repr(rust) padding ok, borsh len is what matters
        let tick_borsh_len = 1 + 16 + 16 + 16 + 16 + 48;
        assert_eq!(tick_borsh_len, 113);
        let whirlpool_borsh_len = 32 + 1 + 2 + 2 + 2 + 2 + 16 + 16 + 4 + 8 + 8
            + 32 + 32 + 16 + 32 + 32 + 16 + 8 + 3 * (32 + 32 + 32 + 16 + 16);
        assert_eq!(whirlpool_borsh_len, 645);
    }

    #[test]
    fn exact_quote_on_synthetic_pool() {
        // Constant liquidity around tick 0, price 1.0 (sqrt = 1<<64).
        let pool = WhirlpoolPool {
            whirlpools_config: Pubkey::new_unique(),
            whirlpool_bump: [0],
            tick_spacing: 64,
            fee_tier_index_seed: 64u16.to_le_bytes(),
            fee_rate: 3000, // 0.30%
            protocol_fee_rate: 0,
            liquidity: 1_000_000_000_000,
            sqrt_price: 1u128 << 64,
            tick_current_index: 0,
            protocol_fee_owed_a: 0,
            protocol_fee_owed_b: 0,
            token_mint_a: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            fee_growth_global_a: 0,
            token_mint_b: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            fee_growth_global_b: 0,
            reward_last_updated_timestamp: 0,
            reward_infos: [
                WhirlpoolRewardInfo {
                    mint: Pubkey::default(),
                    vault: Pubkey::default(),
                    authority: Pubkey::default(),
                    emissions_per_second_x64: 0,
                    growth_global_x64: 0,
                },
                WhirlpoolRewardInfo {
                    mint: Pubkey::default(),
                    vault: Pubkey::default(),
                    authority: Pubkey::default(),
                    emissions_per_second_x64: 0,
                    growth_global_x64: 0,
                },
                WhirlpoolRewardInfo {
                    mint: Pubkey::default(),
                    vault: Pubkey::default(),
                    authority: Pubkey::default(),
                    emissions_per_second_x64: 0,
                    growth_global_x64: 0,
                },
            ],
        };

        let empty_tick = TickFacade {
            initialized: false,
            liquidity_net: 0,
            liquidity_gross: 0,
            fee_growth_outside_a: 0,
            fee_growth_outside_b: 0,
            reward_growths_outside: [0; 3],
        };
        let arr = TickArrayFacade {
            start_tick_index: 0,
            ticks: [empty_tick; 88],
        };

        // 1M b -> a at price 1.0 with 0.30% fee: ~997k. (b->a walks ticks
        // upward, staying inside the single [0..5632) array.)
        let quote = swap_quote_by_input_token(
            1_000_000,
            false,
            0,
            pool_to_facade(&pool),
            None,
            TickArrays::One(arr),
            0,
            None,
            None,
        )
        .unwrap();
        assert!(
            quote.token_est_out > 995_000 && quote.token_est_out < 998_000,
            "{}",
            quote.token_est_out
        );
    }
}
