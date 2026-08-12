//! Verificação dos offsets de liquidez (dataSlice) contra pool accounts reais
//! de mainnet (fixtures fixadas). Se um offset quebrar (mudança de layout
//! on-chain ou erro de conta), o ranking top-N por liquidez vira lixo — este
//! teste é a rede de segurança.
use borsh::BorshDeserialize;

fn raw(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).unwrap()
}

fn u128_at(data: &[u8], off: usize) -> u128 {
    u128::from_le_bytes(data[off..off + 16].try_into().unwrap())
}

fn u64_at(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

#[test]
fn whirlpool_liquidity_offset() {
    let data = raw("wp.raw");
    let pool = orca_whirlpool::WhirlpoolPool::deserialize(&mut &data[8..]).unwrap();
    assert_eq!(u128_at(&data, 49), pool.liquidity, "Whirlpool liquidity não está em data[49..65]");
    assert_eq!(u128_at(&data, 65), pool.sqrt_price, "Whirlpool sqrt_price não está em data[65..81]");
    // mints: token_mint_a em data[101..133], token_mint_b em data[181..213]
    assert_eq!(&data[101..133], pool.token_mint_a.as_ref(), "Whirlpool token_mint_a não está em data[101..133]");
    assert_eq!(&data[181..213], pool.token_mint_b.as_ref(), "Whirlpool token_mint_b não está em data[181..213]");
}

#[test]
fn clmm_liquidity_offset() {
    let data = raw("clmm.raw");
    let pool = raydium_clmm::RaydiumCLMMPool::deserialize(&mut &data[8..]).unwrap();
    assert_eq!(u128_at(&data, 237), pool.liquidity, "Raydium CLMM liquidity não está em data[237..253]");
    assert_eq!(u128_at(&data, 253), pool.sqrt_price_x64, "Raydium CLMM sqrt_price_x64 não está em data[253..269]");
    // mints: token_mint_0 em data[73..105], token_mint_1 em data[105..137]
    assert_eq!(&data[73..105], pool.token_mint_0.as_ref(), "CLMM token_mint_0 não está em data[73..105]");
    assert_eq!(&data[105..137], pool.token_mint_1.as_ref(), "CLMM token_mint_1 não está em data[105..137]");
}

#[test]
fn dammv2_liquidity_offset() {
    let data = raw("v2.raw");
    let pool = meteora_damm::MeteoraDAMMV2Pool::deserialize(&mut &data[8..]).unwrap();
    // mints: token_a_mint em data[168..200], token_b_mint em data[200..232]
    assert_eq!(&data[168..200], pool.token_a_mint.as_ref(), "DAMM V2 token_a_mint não está em data[168..200]");
    assert_eq!(&data[200..232], pool.token_b_mint.as_ref(), "DAMM V2 token_b_mint não está em data[200..232]");
    // reservas cacheadas: token_a_amount em data[680..688], token_b_amount em data[688..696]
    assert_eq!(u64_at(&data, 680), pool.token_a_amount, "DAMM V2 token_a_amount não está em data[680..688]");
    assert_eq!(u64_at(&data, 688), pool.token_b_amount, "DAMM V2 token_b_amount não está em data[688..696]");
}

#[test]
fn v4_vault_offsets() {
    let data = raw("v4.raw");
    // V4 não tem discriminator (struct começa no byte 0).
    let pool = raydium_amm_v4::RaydiumAMMV4::try_from_slice(&data).unwrap();
    // mints: base_mint em data[400..432], quote_mint em data[432..464]
    assert_eq!(&data[400..432], pool.base_mint.as_ref(), "V4 base_mint não está em data[400..432]");
    assert_eq!(&data[432..464], pool.quote_mint.as_ref(), "V4 quote_mint não está em data[432..464]");
    // vaults: base_vault em data[336..368], quote_vault em data[368..400]
    assert_eq!(&data[336..368], pool.base_vault.as_ref(), "V4 base_vault não está em data[336..368]");
    assert_eq!(&data[368..400], pool.quote_vault.as_ref(), "V4 quote_vault não está em data[368..400]");
}

#[test]
fn dammv1_vault_offsets() {
    let data = raw("damm_v1.raw");
    let pool = meteora_damm::MeteoraDAMMPool::deserialize(&mut &data[8..]).unwrap();
    // mints: token_a_mint em data[40..72], token_b_mint em data[72..104]
    assert_eq!(&data[40..72], pool.token_a_mint.as_ref(), "DAMM V1 token_a_mint não está em data[40..72]");
    assert_eq!(&data[72..104], pool.token_b_mint.as_ref(), "DAMM V1 token_b_mint não está em data[72..104]");
    // vault seeds: a_vault em data[104..136], b_vault em data[136..168]
    assert_eq!(&data[104..136], pool.a_vault.as_ref(), "DAMM V1 a_vault não está em data[104..136]");
    assert_eq!(&data[136..168], pool.b_vault.as_ref(), "DAMM V1 b_vault não está em data[136..168]");
}

#[test]
fn dlmm_vault_offsets() {
    let data = raw("dlmm.raw");
    let pool = meteora_dlmm::MeteoraDLMMPool::deserialize(&mut &data[8..]).unwrap();
    // mints: token_x_mint em data[88..120], token_y_mint em data[120..152]
    assert_eq!(&data[88..120], pool.token_x_mint.as_ref(), "DLMM token_x_mint não está em data[88..120]");
    assert_eq!(&data[120..152], pool.token_y_mint.as_ref(), "DLMM token_y_mint não está em data[120..152]");
    // vaults: reserve_x em data[152..184], reserve_y em data[184..216]
    assert_eq!(&data[152..184], pool.reserve_x.as_ref(), "DLMM reserve_x não está em data[152..184]");
    assert_eq!(&data[184..216], pool.reserve_y.as_ref(), "DLMM reserve_y não está em data[184..216]");
}

#[test]
fn pumpfun_vault_offsets() {
    let data = raw("pumpfun.raw");
    let pool = pumpfun_amm::PumpfunAmmPool::deserialize(&mut &data[8..]).unwrap();
    // mints: base_mint em data[43..75], quote_mint em data[75..107]
    assert_eq!(&data[43..75], pool.base_mint.as_ref(), "Pumpfun base_mint não está em data[43..75]");
    assert_eq!(&data[75..107], pool.quote_mint.as_ref(), "Pumpfun quote_mint não está em data[75..107]");
    // vaults: pool_base_token_account em data[139..171], pool_quote_token_account em data[171..203]
    assert_eq!(&data[139..171], pool.pool_base_token_account.as_ref(), "Pumpfun pool_base_token_account não está em data[139..171]");
    assert_eq!(&data[171..203], pool.pool_quote_token_account.as_ref(), "Pumpfun pool_quote_token_account não está em data[171..203]");
}
