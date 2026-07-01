use borsh::BorshDeserialize;

// =============================================================================
// V1 Models
// =============================================================================

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct PoolFees {
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub protocol_trade_fee_numerator: u64,
    pub protocol_trade_fee_denominator: u64,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub enum PoolType {
    Permissioned,
    Permissionless,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct Bootstrapping {
    pub activation_point: u64,
    pub whitelisted_vault: solana_pubkey::Pubkey,
    pub pool_creator: solana_pubkey::Pubkey,
    pub activation_type: u8,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct PartnerInfo {
    pub fee_numerator: u64,
    pub partner_authority: solana_pubkey::Pubkey,
    pub pending_fee_a: u64,
    pub pending_fee_b: u64,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct TokenMultiplier {
    pub token_a_multiplier: u64,
    pub token_b_multiplier: u64,
    pub precision_factor: u8,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub enum DepegType {
    None,
    Marinade,
    Lido,
    SplStake,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct Depeg {
    pub base_virtual_price: u64,
    pub base_cache_updated: u64,
    pub depeg_type: DepegType,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub enum CurveType {
    ConstantProduct,
    Stable {
        amp: u64,
        token_multiplier: TokenMultiplier,
        depeg: Depeg,
        last_amp_updated_timestamp: u64,
    },
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct Padding {
    pub padding0: [u8; 6],
    pub padding1: [u64; 21],
    pub padding2: [u64; 21],
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeteoraDAMMPool {
    pub lp_mint: solana_pubkey::Pubkey,
    pub token_a_mint: solana_pubkey::Pubkey,
    pub token_b_mint: solana_pubkey::Pubkey,
    pub a_vault: solana_pubkey::Pubkey,
    pub b_vault: solana_pubkey::Pubkey,
    pub a_vault_lp: solana_pubkey::Pubkey,
    pub b_vault_lp: solana_pubkey::Pubkey,
    pub a_vault_lp_bump: u8,
    pub enabled: bool,
    pub protocol_token_a_fee: solana_pubkey::Pubkey,
    pub protocol_token_b_fee: solana_pubkey::Pubkey,
    pub fee_last_updated_at: u64,
    pub padding0: [u8; 24],
    pub fees: PoolFees,
    pub pool_type: PoolType,
    pub stake: solana_pubkey::Pubkey,
    pub total_locked_lp: u64,
    pub bootstrapping: Bootstrapping,
    pub partner_info: PartnerInfo,
    pub padding: Padding,
    pub curve_type: CurveType,
}

#[derive(BorshDeserialize, Clone, Debug)]
pub struct Config {
    pub pool_fees: PoolFees,
    pub activation_duration: u64,
    pub vault_config_key: solana_pubkey::Pubkey,
    pub pool_creator_authority: solana_pubkey::Pubkey,
    pub activation_type: u8,
    pub partner_fee_numerator: u64,
    pub padding: [u8; 219],
}

// =============================================================================
// V2 Models
// =============================================================================

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BaseFee {
    pub cliff_fee_numerator: u64,
    pub fee_scheduler_mode: u8,
    pub padding_0: [u8; 5],
    pub number_of_period: u16,
    pub period_frequency: u64,
    pub reduction_factor: u64,
    pub padding_1: u64,
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PoolMetrics {
    pub total_lp_a_fee: u128,
    pub total_lp_b_fee: u128,
    pub total_protocol_a_fee: u64,
    pub total_protocol_b_fee: u64,
    pub total_partner_a_fee: u64,
    pub total_partner_b_fee: u64,
    pub total_position: u64,
    pub padding: u64,
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DynamicFee {
    pub initialized: u8,
    pub padding: [u8; 7],
    pub max_volatility_accumulator: u32,
    pub variable_fee_control: u32,
    pub bin_step: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub last_update_timestamp: u64,
    pub bin_step_u128: u128,
    pub sqrt_price_reference: u128,
    pub volatility_accumulator: u128,
    pub volatility_reference: u128,
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct V2PoolFees {
    pub base_fee: BaseFee,
    pub protocol_fee_percent: u8,
    pub partner_fee_percent: u8,
    pub referral_fee_percent: u8,
    pub padding_0: [u8; 5],
    pub dynamic_fee: DynamicFee,
    pub padding_1: [u64; 2],
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewardInfo {
    pub initialized: u8,
    pub reward_token_flag: u8,
    pub _padding_0: [u8; 6],
    pub _padding_1: [u8; 8],
    pub mint: solana_pubkey::Pubkey,
    pub vault: solana_pubkey::Pubkey,
    pub funder: solana_pubkey::Pubkey,
    pub reward_duration: u64,
    pub reward_duration_end: u64,
    pub reward_rate: u128,
    pub reward_per_token_stored: [u8; 32],
    pub last_update_time: u64,
    pub cumulative_seconds_with_empty_liquidity_reward: u64,
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeteoraDAMMV2Pool {
    pub pool_fees: V2PoolFees,
    pub token_a_mint: solana_pubkey::Pubkey,
    pub token_b_mint: solana_pubkey::Pubkey,
    pub token_a_vault: solana_pubkey::Pubkey,
    pub token_b_vault: solana_pubkey::Pubkey,
    pub whitelisted_vault: solana_pubkey::Pubkey,
    pub partner: solana_pubkey::Pubkey,
    pub liquidity: u128,
    pub _padding: u128,
    pub protocol_a_fee: u64,
    pub protocol_b_fee: u64,
    pub partner_a_fee: u64,
    pub partner_b_fee: u64,
    pub sqrt_min_price: u128,
    pub sqrt_max_price: u128,
    pub sqrt_price: u128,
    pub activation_point: u64,
    pub activation_type: u8,
    pub pool_status: u8,
    pub token_a_flag: u8,
    pub token_b_flag: u8,
    pub collect_fee_mode: u8,
    pub pool_type: u8,
    pub _padding_0: [u8; 2],
    pub fee_a_per_liquidity: [u8; 32],
    pub fee_b_per_liquidity: [u8; 32],
    pub permanent_lock_liquidity: u128,
    pub metrics: PoolMetrics,
    pub creator: solana_pubkey::Pubkey,
    pub token_a_amount: u64,
    pub token_b_amount: u64,
    pub layout_version: u8,
    pub _padding_3: [u8; 7],
    pub _padding_4: [u64; 3],
    pub reward_infos: [RewardInfo; 2],
}

// =============================================================================
// Vault Authority Models
// =============================================================================

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct VaultBumps {
    pub vault_bump: u8,
    pub token_vault_bump: u8,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct LockedProfitTracker {
    pub last_updated_locked_profit: u64,
    pub last_report: u64,
    pub locked_profile_degradation: u8,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct VaultAuthority {
    pub enabled: [u8; 1],
    pub bumps: VaultBumps,
    pub total_amount: u64,
    pub token_vault: solana_pubkey::Pubkey,
    pub fee_vault: solana_pubkey::Pubkey,
    pub token_mint: solana_pubkey::Pubkey,
    pub lp_mint: solana_pubkey::Pubkey,
    pub strategies: [solana_pubkey::Pubkey; 30],
    pub base: solana_pubkey::Pubkey,
    pub admin: solana_pubkey::Pubkey,
    pub operator: solana_pubkey::Pubkey,
    pub locked_profit_tracker: LockedProfitTracker,
}
