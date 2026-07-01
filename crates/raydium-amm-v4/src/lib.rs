
use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use thunder_core::{
    GenericError, Market, SwapDirection, PoolMetadata, PoolFinancials, PoolFees,
    constant_product_swap, calculate_price_impact_bps,
    quote_priority,
};

// ---------------------------------------------------------------------------
// DEX-specific constants
// ---------------------------------------------------------------------------

pub const RAYDIUM_LIQUIDITY_POOL_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const RAYDIUM_AUTHORITY_V4: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";

// ---------------------------------------------------------------------------
// On-chain models
// ---------------------------------------------------------------------------




#[derive(Debug, BorshDeserialize, Clone, serde::Serialize, serde::Deserialize)]
pub struct RaydiumAMMV4 {
    pub status: u64,
    pub nonce: u64,
    pub max_order: u64,
    pub depth: u64,
    pub base_decimal: u64,
    pub quote_decimal: u64,
    pub state: u64,
    pub reset_flag: u64,
    pub min_size: u64,
    pub vol_max_cut_ratio: u64,
    pub amount_wave: u64,
    pub base_lot_size: u64,
    pub quote_lot_size: u64,
    pub min_price_multiplier: u64,
    pub max_price_multiplier: u64,
    pub system_decimal_value: u64,
    pub min_separate_numerator: u64,
    pub min_separate_denominator: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub pnl_numerator: u64,
    pub pnl_denominator: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
    pub base_need_take_pnl: u64,
    pub quote_need_take_pnl: u64,
    pub quote_total_pnl: u64,
    pub base_total_pnl: u64,
    pub pool_open_time: u64,
    pub punish_pc_amount: u64,
    pub punish_coin_amount: u64,
    pub orderbook_to_init_time: u64,
    pub swap_base_in_amount: u128,
    pub swap_quote_out_amount: u128,
    pub swap_base_2_quote_fee: u64,
    pub swap_quote_in_amount: u128,
    pub swap_base_out_amount: u128,
    pub swap_quote_2_base_fee: u64,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub open_orders: Pubkey,
    pub market_id: Pubkey,
    pub market_program_id: Pubkey,
    pub target_oders: Pubkey,
    pub withdraw_queue: Pubkey,
    pub lp_vault: Pubkey,
    pub owner: Pubkey,
    pub lp_reserve: u64,
    pub padding: [u64; 3],
}



// ---------------------------------------------------------------------------
// Market wrapper
// ---------------------------------------------------------------------------

pub struct RaydiumAmmV4Market {
    pub pool: RaydiumAMMV4,
    pub pool_address: String,
    pub quote_balance: u64,
    pub base_balance: u64,
    pub flipped: bool,
}

impl RaydiumAmmV4Market {
    pub fn new(
        pool: RaydiumAMMV4,
        pool_address: String,
        quote_balance: u64,
        base_balance: u64,
    ) -> Self {
        // Pool struct has explicit base_mint/quote_mint, but they can be swapped.
        // If base_mint is a quote currency (WSOL/USDC) and quote_mint is not,
        // the pool's labels are backwards relative to our convention.
        let flipped = quote_priority(&pool.base_mint).unwrap_or(usize::MAX) < quote_priority(&pool.quote_mint).unwrap_or(usize::MAX);
        Self { pool, pool_address, quote_balance, base_balance, flipped }
    }
}

// ---------------------------------------------------------------------------
// Market trait implementation
// ---------------------------------------------------------------------------

impl Market for RaydiumAmmV4Market {
    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        let fee_bps = (self.pool.trade_fee_numerator as f64
            / self.pool.trade_fee_denominator as f64
            * 10000.0) as u64;

        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Raydium AMM V4".to_string(),
            quote_mint: if self.flipped { self.pool.base_mint } else { self.pool.quote_mint },
            base_mint: if self.flipped { self.pool.quote_mint } else { self.pool.base_mint },
            quote_vault: if self.flipped { self.pool.base_vault } else { self.pool.quote_vault },
            base_vault: if self.flipped { self.pool.quote_vault } else { self.pool.base_vault },
            fees: PoolFees {
                trade_fee_bps: fee_bps,
                protocol_fee_bps: None,
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        Ok(PoolFinancials {
            quote_balance: if self.flipped { self.base_balance } else { self.quote_balance },
            base_balance: if self.flipped { self.quote_balance } else { self.base_balance },
            quote_decimals: if self.flipped { self.pool.base_decimal as u8 } else { self.pool.quote_decimal as u8 },
            base_decimals: if self.flipped { self.pool.quote_decimal as u8 } else { self.pool.base_decimal as u8 },
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let fee_bps = (self.pool.trade_fee_numerator as f64
            / self.pool.trade_fee_denominator as f64
            * 10000.0) as u64;

        // When flipped, the pool's physical quote/base are swapped relative to
        // our normalized convention, so we swap reserve_in/reserve_out.
        let (quote_bal, base_bal) = if self.flipped {
            (self.base_balance, self.quote_balance)
        } else {
            (self.quote_balance, self.base_balance)
        };

        match direction {
            SwapDirection::Buy => {
                // Quote -> Base
                constant_product_swap(quote_bal, base_bal, amount_in, fee_bps)
            }
            SwapDirection::Sell => {
                // Base -> Quote
                constant_product_swap(base_bal, quote_bal, amount_in, fee_bps)
            }
        }
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        _pool_data: Option<&[u8]>,
        quote_vault_balance: u64,
        base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        if _pool_data.is_none() {
            return self.calculate_output(amount_in, direction);
        }

        let fee_bps = (self.pool.trade_fee_numerator as f64
            / self.pool.trade_fee_denominator as f64
            * 10000.0) as u64;

        // Same flipping logic as calculate_output: when flipped, the caller's
        // normalized quote/base are already swapped, but the swap math expects
        // physical orientation, so we swap them back.
        let (quote_bal, base_bal) = if self.flipped {
            (base_vault_balance, quote_vault_balance)
        } else {
            (quote_vault_balance, base_vault_balance)
        };

        match direction {
            SwapDirection::Buy => {
                // Quote -> Base
                constant_product_swap(quote_bal, base_bal, amount_in, fee_bps)
            }
            SwapDirection::Sell => {
                // Base -> Quote
                constant_product_swap(base_bal, quote_bal, amount_in, fee_bps)
            }
        }
    }

    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let pre_swap_price = self.current_price()?;
        let output = self.calculate_output(amount_in, direction)?;

        let (quote_bal, base_bal) = if self.flipped {
            (self.base_balance, self.quote_balance)
        } else {
            (self.quote_balance, self.base_balance)
        };

        let post_swap_price = match direction {
            SwapDirection::Buy => {
                let new_quote = quote_bal + amount_in;
                let new_base = base_bal - output;
                new_quote as f64 / new_base as f64
            }
            SwapDirection::Sell => {
                let new_base = base_bal + amount_in;
                let new_quote = quote_bal - output;
                new_quote as f64 / new_base as f64
            }
        };

        Ok(calculate_price_impact_bps(pre_swap_price, post_swap_price))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let (quote_bal, base_bal) = if self.flipped {
            (self.base_balance, self.quote_balance)
        } else {
            (self.quote_balance, self.base_balance)
        };
        if base_bal == 0 {
            return Err("Pool has zero base balance".into());
        }
        let (quote_dec, base_dec) = if self.flipped {
            (self.pool.base_decimal, self.pool.quote_decimal)
        } else {
            (self.pool.quote_decimal, self.pool.base_decimal)
        };
        let quote_human = quote_bal as f64 / 10f64.powi(quote_dec as i32);
        let base_human = base_bal as f64 / 10f64.powi(base_dec as i32);
        Ok(quote_human / base_human)
    }

    fn is_active(&self) -> bool {
        self.pool.status == 6
    }
}
