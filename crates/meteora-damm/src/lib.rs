pub mod models;
pub mod utils;

pub use models::{
    MeteoraDAMMPool, MeteoraDAMMV2Pool, VaultAuthority, CurveType, Config,
    V2PoolFees, BaseFee, PoolMetrics, DynamicFee, RewardInfo,
};
pub use utils::derive_token_vault_address;

use thunder_core::{
    GenericError, Market, PoolFinancials, PoolFees, PoolMetadata,
    SwapDirection, calculate_price_impact_bps, constant_product_swap, infer_mint_decimals, quote_priority,
};

// =============================================================================
// Constants
// =============================================================================

pub const METEORA_DYNAMIC_AMM: &str = "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB";
pub const METEORA_DYNAMIC_AMM_V2: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";
pub const METEORA_VAULT_PROGRAM: &str = "24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi";


// =============================================================================
// V1: MeteoraDAMMMarket
// =============================================================================

/// Market wrapper for Meteora Dynamic AMM V1 pools.
pub struct MeteoraDAMMMarket {
    pub pool: MeteoraDAMMPool,
    pub pool_address: String,
    /// Cached vault balances (updated externally after fetching on-chain data).
    pub a_vault_balance: u64,
    pub b_vault_balance: u64,
    pub token_a_decimals: u8,
    pub token_b_decimals: u8,
    /// True when the pool's token_a is actually the quote currency (WSOL/USDC),
    /// meaning the default quote=token_b / base=token_a mapping is inverted.
    pub flipped: bool,
}

impl MeteoraDAMMMarket {
    pub fn new(pool: MeteoraDAMMPool, pool_address: String) -> Self {
        let flipped = quote_priority(&pool.token_a_mint).unwrap_or(usize::MAX) < quote_priority(&pool.token_b_mint).unwrap_or(usize::MAX);
        let token_a_decimals = infer_mint_decimals(&pool.token_a_mint);
        let token_b_decimals = infer_mint_decimals(&pool.token_b_mint);
        Self {
            pool,
            pool_address,
            a_vault_balance: 0,
            b_vault_balance: 0,
            token_a_decimals,
            token_b_decimals,
            flipped,
        }
    }

    /// Calculate output based on curve type (constant product or stable).
    fn calculate_damm_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let fee_bps = (self.pool.fees.trade_fee_numerator as f64
            / self.pool.fees.trade_fee_denominator as f64
            * 10000.0) as u64;

        // Normalize direction: if flipped, a Buy in market terms is physically
        // selling token_a (the physical quote) to get token_b, which is the
        // reverse of the un-flipped path.
        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        match &self.pool.curve_type {
            CurveType::ConstantProduct => match physical_direction {
                SwapDirection::Buy => constant_product_swap(
                    self.b_vault_balance,
                    self.a_vault_balance,
                    amount_in,
                    fee_bps,
                ),
                SwapDirection::Sell => constant_product_swap(
                    self.a_vault_balance,
                    self.b_vault_balance,
                    amount_in,
                    fee_bps,
                ),
            },
            CurveType::Stable { amp, .. } => {
                let base_output = match physical_direction {
                    SwapDirection::Buy => constant_product_swap(
                        self.b_vault_balance,
                        self.a_vault_balance,
                        amount_in,
                        fee_bps,
                    )?,
                    SwapDirection::Sell => constant_product_swap(
                        self.a_vault_balance,
                        self.b_vault_balance,
                        amount_in,
                        fee_bps,
                    )?,
                };

                let amp_factor = (*amp as f64 / 100.0).min(10.0);
                let stable_output = (base_output as f64 * (1.0 + amp_factor / 100.0)) as u64;

                // Cap at available reserves.
                Ok(stable_output.min(match physical_direction {
                    SwapDirection::Buy => self.a_vault_balance,
                    SwapDirection::Sell => self.b_vault_balance,
                }))
            }
        }
    }
}

impl Market for MeteoraDAMMMarket {
    fn is_active(&self) -> bool {
        self.pool.enabled
    }

    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        let fee_bps = (self.pool.fees.trade_fee_numerator as f64
            / self.pool.fees.trade_fee_denominator as f64
            * 10000.0) as u64;

        let protocol_fee_bps = (self.pool.fees.protocol_trade_fee_numerator as f64
            / self.pool.fees.protocol_trade_fee_denominator as f64
            * 10000.0) as u64;

        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: match self.pool.curve_type {
                CurveType::ConstantProduct => "Meteora DAMM (Constant Product)".to_string(),
                CurveType::Stable { .. } => "Meteora DAMM (Stable)".to_string(),
            },
            quote_mint: if self.flipped { self.pool.token_a_mint } else { self.pool.token_b_mint },
            base_mint: if self.flipped { self.pool.token_b_mint } else { self.pool.token_a_mint },
            quote_vault: if self.flipped { derive_token_vault_address(self.pool.a_vault).0 } else { derive_token_vault_address(self.pool.b_vault).0 },
            base_vault: if self.flipped { derive_token_vault_address(self.pool.b_vault).0 } else { derive_token_vault_address(self.pool.a_vault).0 },
            fees: PoolFees {
                trade_fee_bps: fee_bps,
                protocol_fee_bps: Some(protocol_fee_bps),
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        Ok(PoolFinancials {
            quote_balance: if self.flipped { self.a_vault_balance } else { self.b_vault_balance },
            base_balance: if self.flipped { self.b_vault_balance } else { self.a_vault_balance },
            quote_decimals: if self.flipped { self.token_a_decimals } else { self.token_b_decimals },
            base_decimals: if self.flipped { self.token_b_decimals } else { self.token_a_decimals },
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        self.calculate_damm_output(amount_in, direction)
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

        let fee_bps = (self.pool.fees.trade_fee_numerator as f64
            / self.pool.fees.trade_fee_denominator as f64
            * 10000.0) as u64;

        // Reconstruct physical (a_vault, b_vault) from normalized (quote, base).
        // When !flipped: quote=b_vault, base=a_vault.
        // When  flipped: quote=a_vault, base=b_vault.
        let (a_vault_balance, b_vault_balance) = if self.flipped {
            (quote_vault_balance, base_vault_balance)
        } else {
            (base_vault_balance, quote_vault_balance)
        };

        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        match &self.pool.curve_type {
            CurveType::ConstantProduct => match physical_direction {
                SwapDirection::Buy => constant_product_swap(
                    b_vault_balance,
                    a_vault_balance,
                    amount_in,
                    fee_bps,
                ),
                SwapDirection::Sell => constant_product_swap(
                    a_vault_balance,
                    b_vault_balance,
                    amount_in,
                    fee_bps,
                ),
            },
            CurveType::Stable { amp, .. } => {
                let base_output = match physical_direction {
                    SwapDirection::Buy => constant_product_swap(
                        b_vault_balance,
                        a_vault_balance,
                        amount_in,
                        fee_bps,
                    )?,
                    SwapDirection::Sell => constant_product_swap(
                        a_vault_balance,
                        b_vault_balance,
                        amount_in,
                        fee_bps,
                    )?,
                };

                let amp_factor = (*amp as f64 / 100.0).min(10.0);
                let stable_output = (base_output as f64 * (1.0 + amp_factor / 100.0)) as u64;

                // Cap at available reserves.
                Ok(stable_output.min(match physical_direction {
                    SwapDirection::Buy => a_vault_balance,
                    SwapDirection::Sell => b_vault_balance,
                }))
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

        // Price impact uses physical direction (already normalized in calculate_output).
        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        let post_swap_price = match physical_direction {
            SwapDirection::Buy => {
                let new_b = self.b_vault_balance + amount_in;
                let new_a = self.a_vault_balance.saturating_sub(output);
                if new_a == 0 {
                    return Err("Insufficient liquidity in pool".into());
                }
                new_b as f64 / new_a as f64
            }
            SwapDirection::Sell => {
                let new_a = self.a_vault_balance + amount_in;
                let new_b = self.b_vault_balance.saturating_sub(output);
                if new_a == 0 {
                    return Err("Insufficient liquidity in pool".into());
                }
                new_b as f64 / new_a as f64
            }
        };

        Ok(calculate_price_impact_bps(pre_swap_price, post_swap_price))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let (quote_bal, base_bal, quote_dec, base_dec) = if self.flipped {
            (self.a_vault_balance, self.b_vault_balance, self.token_a_decimals, self.token_b_decimals)
        } else {
            (self.b_vault_balance, self.a_vault_balance, self.token_b_decimals, self.token_a_decimals)
        };
        if base_bal == 0 {
            return Err("Pool has zero base balance".into());
        }
        let quote_human = quote_bal as f64 / 10f64.powi(quote_dec as i32);
        let base_human = base_bal as f64 / 10f64.powi(base_dec as i32);
        Ok(quote_human / base_human)
    }
}


// =============================================================================
// V2: MeteoraDAMMV2Market
// =============================================================================

/// Market wrapper for Meteora Dynamic AMM V2 pools.
pub struct MeteoraDAMMV2Market {
    pub pool: MeteoraDAMMV2Pool,
    pub pool_address: String,
    pub a_vault_balance: u64,
    pub b_vault_balance: u64,
    /// Decimals for token_a and token_b. Needed for sqrt_price conversion.
    /// Defaults to (6, 9) if not provided (assumes token_a=token, token_b=SOL).
    pub token_a_decimals: u8,
    pub token_b_decimals: u8,
    /// True when the pool's token_a is actually the quote currency (WSOL/USDC).
    pub flipped: bool,
}

impl MeteoraDAMMV2Market {
    pub fn new(pool: MeteoraDAMMV2Pool, pool_address: String) -> Self {
        let flipped = quote_priority(&pool.token_a_mint).unwrap_or(usize::MAX) < quote_priority(&pool.token_b_mint).unwrap_or(usize::MAX);
        let da = infer_mint_decimals(&pool.token_a_mint);
        let db = infer_mint_decimals(&pool.token_b_mint);
        Self {
            pool,
            pool_address,
            a_vault_balance: 0,
            b_vault_balance: 0,
            token_a_decimals: da,
            token_b_decimals: db,
            flipped,
        }
    }


    /// Convert V2 Q64.128 sqrt_price to a regular price (token_a per token_b).
    ///
    /// Formula: price = (sqrt_price >> 64)^2 * 10^(decimals_a - decimals_b)
    /// The sqrt_price is Q64.128: upper 64 bits are integer, lower 64 bits are fraction.
    fn sqrt_price_to_price(&self, decimals_a: u8, decimals_b: u8) -> f64 {
        // Extract the rational value: sqrt_price / 2^64
        let sqrt_price_f64 = self.pool.sqrt_price as f64 / (1u128 << 64) as f64;
        let raw = sqrt_price_f64 * sqrt_price_f64;
        // Apply decimal adjustment
        let decimal_adj = 10f64.powi(decimals_a as i32 - decimals_b as i32);
        raw * decimal_adj
    }

    /// Base fee in basis points.
    /// cliff_fee_numerator is in parts-per-billion (1e9).
    fn calculate_base_fee_bps(&self) -> u64 {
        // cliff_fee_numerator / 1e9 gives the fraction, * 10000 gives bps
        self.pool.pool_fees.base_fee.cliff_fee_numerator / 100_000
    }

    /// Simplified V2 output calculation using current sqrt_price.
    fn calculate_v2_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let price = self.sqrt_price_to_price(self.token_a_decimals, self.token_b_decimals);
        let fee_bps = self.calculate_base_fee_bps();

        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        // Normalize direction when flipped.
        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        // Cap at available reserves — can't output more than the pool holds.
        let output = match physical_direction {
            SwapDirection::Buy => {
                let raw = (amount_in_with_fee as f64 / price) as u64;
                raw.min(self.a_vault_balance)
            }
            SwapDirection::Sell => {
                let raw = (amount_in_with_fee as f64 * price) as u64;
                raw.min(self.b_vault_balance)
            }
        };

        Ok(output)
    }
}

impl Market for MeteoraDAMMV2Market {
    fn is_active(&self) -> bool {
        self.pool.pool_status == 0
    }

    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        let trade_fee_bps = self.calculate_base_fee_bps();
        let protocol_fee_bps =
            (self.pool.pool_fees.protocol_fee_percent as u64 * trade_fee_bps) / 100;

        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Meteora DAMM V2".to_string(),
            quote_mint: if self.flipped { self.pool.token_a_mint } else { self.pool.token_b_mint },
            base_mint: if self.flipped { self.pool.token_b_mint } else { self.pool.token_a_mint },
            quote_vault: if self.flipped { self.pool.token_a_vault } else { self.pool.token_b_vault },
            base_vault: if self.flipped { self.pool.token_b_vault } else { self.pool.token_a_vault },
            fees: PoolFees {
                trade_fee_bps,
                protocol_fee_bps: Some(protocol_fee_bps),
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        Ok(PoolFinancials {
            quote_balance: if self.flipped { self.a_vault_balance } else { self.b_vault_balance },
            base_balance: if self.flipped { self.b_vault_balance } else { self.a_vault_balance },
            quote_decimals: if self.flipped { self.token_a_decimals } else { self.token_b_decimals },
            base_decimals: if self.flipped { self.token_b_decimals } else { self.token_a_decimals },
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        self.calculate_v2_output(amount_in, direction)
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        quote_vault_balance: u64,
        base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        // Reconstruct physical vault balances from normalized inputs.
        let (a_vault_balance, b_vault_balance) = if self.flipped {
            (quote_vault_balance, base_vault_balance)
        } else {
            (base_vault_balance, quote_vault_balance)
        };

        // Parse live sqrt_price from pool_data if available, else use cached.
        let sqrt_price: u128 = match pool_data {
            Some(data) if data.len() >= 472 => {
                let bytes: [u8; 16] = data[456..472].try_into().unwrap();
                u128::from_le_bytes(bytes)
            }
            _ => self.pool.sqrt_price,
        };

        let sqrt_price_f64 = sqrt_price as f64 / (1u128 << 64) as f64;
        let price = sqrt_price_f64 * sqrt_price_f64
            * 10f64.powi(self.token_a_decimals as i32 - self.token_b_decimals as i32);

        let fee_bps = self.calculate_base_fee_bps();
        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        let output = match physical_direction {
            SwapDirection::Buy => {
                let raw = (amount_in_with_fee as f64 / price) as u64;
                raw.min(a_vault_balance)
            }
            SwapDirection::Sell => {
                let raw = (amount_in_with_fee as f64 * price) as u64;
                raw.min(b_vault_balance)
            }
        };

        Ok(output)
    }

    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let pre_swap_price = self.current_price()?;
        let output = self.calculate_output(amount_in, direction)?;

        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        let post_swap_price = match physical_direction {
            SwapDirection::Buy => {
                let new_b = self.b_vault_balance + amount_in;
                let new_a = self.a_vault_balance.saturating_sub(output);
                if new_a == 0 {
                    return Err("Insufficient liquidity in pool".into());
                }
                new_b as f64 / new_a as f64
            }
            SwapDirection::Sell => {
                let new_a = self.a_vault_balance + amount_in;
                let new_b = self.b_vault_balance.saturating_sub(output);
                if new_a == 0 {
                    return Err("Insufficient liquidity in pool".into());
                }
                new_b as f64 / new_a as f64
            }
        };

        Ok(calculate_price_impact_bps(pre_swap_price, post_swap_price))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        // Prefer on-chain token amounts when available (layout_version >= 1).
        // These are the actual reserves, no sqrt_price math needed.
        if self.pool.token_a_amount > 0 && self.pool.token_b_amount > 0 {
            let a_human = self.pool.token_a_amount as f64 / 10f64.powi(self.token_a_decimals as i32);
            let b_human = self.pool.token_b_amount as f64 / 10f64.powi(self.token_b_decimals as i32);
            // a_human/b_human = token_a per token_b (human-readable).
            // NOT flipped (quote=b, base=a): want b_per_a = b_human / a_human.
            // Flipped (quote=a, base=b): want a_per_b = a_human / b_human.
            return if self.flipped {
                Ok(a_human / b_human)
            } else {
                Ok(b_human / a_human)
            };
        }

        // Fallback to sqrt_price for older pools.
        let raw = self.sqrt_price_to_price(self.token_a_decimals, self.token_b_decimals);
        if self.flipped {
            Ok(raw)
        } else {
            if raw == 0.0 {
                return Err("Pool has zero price".into());
            }
            Ok(1.0 / raw)
        }
    }
}