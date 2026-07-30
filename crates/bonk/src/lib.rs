//! bonk.fun / Raydium LaunchLab bonding curve (pre-graduation).
//!
//! Program `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj`. Pre-bond tokens trade
//! against virtual + real reserves via constant product with protocol + platform
//! + share fees. Once the fundraising target is hit the pool migrates to an AMM
//! (Raydium) and is no longer traded on the curve — route only `status == Fund`.
//!
//! Unlike pump.fun's bonding curve, the `PoolState` account carries both mints,
//! vaults, and creator, so these pools CAN be enumerated via getProgramAccounts.
//!
//! Math + fees ported from sol-trade-sdk `utils/calc/bonk.rs`
//! (PROTOCOL 25 + PLATFORM 100 + SHARE 0 = 125 bps): buy deducts fees from the
//! quote input, sell deducts them from the quote output.

use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use solroute_core::{
    GenericError, Market, PoolFees, PoolFinancials, PoolMetadata, SwapDirection,
    calculate_price_impact_bps,
};

pub const BONK_LAUNCHPAD_PROGRAM: &str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";

/// `PoolState` Anchor discriminator (`sha256("account:PoolState")[..8]`). Shared
/// with Raydium CLMM's PoolState — disambiguated by program id at load time.
pub const DISC_POOL_STATE: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];

/// Fees (basis points). Standard LaunchLab rates.
pub const PROTOCOL_FEE_RATE: u128 = 25;
pub const PLATFORM_FEE_RATE: u128 = 100;
pub const SHARE_FEE_RATE: u128 = 0;

/// `PoolStatus::Fund` — the fundraising / on-curve phase (tradeable here).
pub const POOL_STATUS_FUND: u8 = 0;

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VestingSchedule {
    pub total_locked_amount: u64,
    pub cliff_period: u64,
    pub unlock_period: u64,
    pub start_time: u64,
    pub allocated_share_amount: u64,
}

/// LaunchLab pool account. Field order matches the on-chain borsh layout; parse
/// with [`parse_pool_state`] (skips the 8-byte discriminator, ignores trailing
/// padding).
#[derive(BorshDeserialize, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BonkPoolState {
    pub epoch: u64,
    pub auth_bump: u8,
    pub status: u8,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub migrate_type: u8,
    pub supply: u64,
    pub total_base_sell: u64,
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub real_base: u64,
    pub real_quote: u64,
    pub total_quote_fund_raising: u64,
    pub quote_protocol_fee: u64,
    pub platform_fee: u64,
    pub migrate_fee: u64,
    pub vesting_schedule: VestingSchedule,
    pub global_config: Pubkey,
    pub platform_config: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub creator: Pubkey,
    // trailing padding [u64; 8] ignored by `deserialize`
}

/// Derive a pool's `PoolState` address (`["pool", base_mint, quote_mint]`).
pub fn derive_pool_pda(base_mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"pool", base_mint.as_ref(), quote_mint.as_ref()],
        &Pubkey::from_str_const(BONK_LAUNCHPAD_PROGRAM),
    )
    .0
}

/// Parse a LaunchLab `PoolState` account (data includes the 8-byte disc).
pub fn parse_pool_state(data: &[u8]) -> Option<BonkPoolState> {
    if data.len() < 8 || data[..8] != DISC_POOL_STATE {
        return None;
    }
    BonkPoolState::deserialize(&mut &data[8..]).ok()
}

#[inline]
fn total_fee(amount: u128) -> u128 {
    // Per-fee flooring, matching sol-trade-sdk exactly.
    let protocol = amount * PROTOCOL_FEE_RATE / 10_000;
    let platform = amount * PLATFORM_FEE_RATE / 10_000;
    let share = amount * SHARE_FEE_RATE / 10_000;
    protocol + platform + share
}

/// Tokens out for spending `amount_in` quote lamports (fees off the input).
pub fn buy_base_out(
    virtual_base: u128,
    virtual_quote: u128,
    real_base: u128,
    real_quote: u128,
    amount_in: u64,
) -> u64 {
    let amount = amount_in as u128;
    let net = amount.saturating_sub(total_fee(amount));
    let input_reserve = virtual_quote + real_quote;
    let output_reserve = virtual_base.saturating_sub(real_base);
    if output_reserve == 0 {
        return 0;
    }
    (net * output_reserve / (input_reserve + net)).min(u64::MAX as u128) as u64
}

/// Quote lamports out for selling `amount_in` base units (fees off the output).
pub fn sell_quote_out(
    virtual_base: u128,
    virtual_quote: u128,
    real_base: u128,
    real_quote: u128,
    amount_in: u64,
) -> u64 {
    let amount = amount_in as u128;
    let input_reserve = virtual_base.saturating_sub(real_base);
    let output_reserve = virtual_quote + real_quote;
    let gross = amount * output_reserve / (input_reserve + amount);
    gross.saturating_sub(total_fee(gross)).min(u64::MAX as u128) as u64
}

pub struct BonkMarket {
    pub pool: BonkPoolState,
    pub pool_address: String,
}

impl BonkMarket {
    pub fn new(pool: BonkPoolState, pool_address: String) -> Self {
        Self { pool, pool_address }
    }
}

impl Market for BonkMarket {
    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Bonk".to_string(),
            quote_mint: self.pool.quote_mint,
            base_mint: self.pool.base_mint,
            quote_vault: self.pool.quote_vault,
            base_vault: self.pool.base_vault,
            fees: PoolFees {
                trade_fee_bps: (PROTOCOL_FEE_RATE + PLATFORM_FEE_RATE + SHARE_FEE_RATE) as u64,
                protocol_fee_bps: Some(PROTOCOL_FEE_RATE as u64),
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        Ok(PoolFinancials {
            quote_balance: self.pool.real_quote,
            base_balance: self.pool.virtual_base.saturating_sub(self.pool.real_base),
            quote_decimals: self.pool.quote_decimals,
            base_decimals: self.pool.base_decimals,
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        if self.pool.status != POOL_STATUS_FUND {
            return Err("bonk pool not in fundraising phase (migrated)".into());
        }
        let p = &self.pool;
        let out = match direction {
            SwapDirection::Buy => buy_base_out(
                p.virtual_base as u128,
                p.virtual_quote as u128,
                p.real_base as u128,
                p.real_quote as u128,
                amount_in,
            ),
            SwapDirection::Sell => sell_quote_out(
                p.virtual_base as u128,
                p.virtual_quote as u128,
                p.real_base as u128,
                p.real_quote as u128,
                amount_in,
            ),
        };
        if out == 0 {
            return Err("bonk produced zero output".into());
        }
        Ok(out)
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        // Re-parse live reserves from streamed pool bytes; fall back to baked.
        if let Some(data) = pool_data {
            if let Some(p) = parse_pool_state(data) {
                let m = BonkMarket::new(p, self.pool_address.clone());
                return m.calculate_output(amount_in, direction);
            }
        }
        self.calculate_output(amount_in, direction)
    }

    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let pre = self.current_price()?;
        let out = self.calculate_output(amount_in, direction)? as u128;
        let p = &self.pool;
        let (vq, vb) = (
            p.virtual_quote as u128 + p.real_quote as u128,
            p.virtual_base as u128 - p.real_base as u128,
        );
        let (new_q, new_b) = match direction {
            SwapDirection::Buy => (vq + amount_in as u128, vb.saturating_sub(out)),
            SwapDirection::Sell => (vq.saturating_sub(out), vb + amount_in as u128),
        };
        if new_b == 0 {
            return Err("bonk pool exhausted".into());
        }
        let scale = 10f64.powi(p.base_decimals as i32 - p.quote_decimals as i32);
        let post = (new_q as f64 / new_b as f64) * scale;
        Ok(calculate_price_impact_bps(pre, post))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let p = &self.pool;
        let vb = (p.virtual_base as u128).saturating_sub(p.real_base as u128);
        if vb == 0 {
            return Err("bonk pool has zero base reserve".into());
        }
        let vq = p.virtual_quote as u128 + p.real_quote as u128;
        let scale = 10f64.powi(p.base_decimals as i32 - p.quote_decimals as i32);
        Ok((vq as f64 / vb as f64) * scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> BonkPoolState {
        BonkPoolState {
            epoch: 0,
            auth_bump: 0,
            status: POOL_STATUS_FUND,
            base_decimals: 6,
            quote_decimals: 9,
            migrate_type: 0,
            supply: 1_000_000_000_000_000,
            total_base_sell: 793_100_000_000_000,
            virtual_base: 1_073_000_000_000_000,
            virtual_quote: 30_000_000_000,
            real_base: 0,
            real_quote: 0,
            total_quote_fund_raising: 85_000_000_000,
            quote_protocol_fee: 0,
            platform_fee: 0,
            migrate_fee: 0,
            vesting_schedule: VestingSchedule::default(),
            global_config: Pubkey::default(),
            platform_config: Pubkey::default(),
            base_mint: Pubkey::new_from_array([2; 32]),
            quote_mint: Pubkey::from_str_const(solroute_core::WSOL),
            base_vault: Pubkey::new_from_array([3; 32]),
            quote_vault: Pubkey::new_from_array([4; 32]),
            creator: Pubkey::new_from_array([5; 32]),
        }
    }

    #[test]
    fn buy_matches_sdk_cp_with_125bps() {
        let out = buy_base_out(1_073_000_000_000_000, 30_000_000_000, 0, 0, 1_000_000_000);
        // net = 1e9 - (25+100+0)bps
        let net = 1_000_000_000u128 - (1_000_000_000 * 25 / 10000) - (1_000_000_000 * 100 / 10000);
        let expected = (net * 1_073_000_000_000_000u128 / (30_000_000_000u128 + net)) as u64;
        assert_eq!(out, expected);
        assert!(out > 0);
    }

    #[test]
    fn migrated_pool_rejected() {
        let mut p = pool();
        p.status = 1; // Migrate
        let m = BonkMarket::new(p, "x".into());
        assert!(m.calculate_output(1_000_000_000, SwapDirection::Buy).is_err());
    }

    #[test]
    fn buy_then_price_monotonic() {
        let m = BonkMarket::new(pool(), "x".into());
        let a = m.calculate_output(1_000_000_000, SwapDirection::Buy).unwrap();
        let b = m.calculate_output(2_000_000_000, SwapDirection::Buy).unwrap();
        assert!(b > a); // more SOL in -> more tokens out
    }

    #[test]
    fn parse_rejects_bad_disc() {
        let mut data = vec![0u8; 400];
        data[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(parse_pool_state(&data).is_none());
    }
}
