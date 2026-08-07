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
