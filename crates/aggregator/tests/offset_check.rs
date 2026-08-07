//! Verificação dos offsets de liquidez (dataSlice) contra pool accounts reais
//! de mainnet (fixtures fixadas). Se um offset quebrar (mudança de layout
//! on-chain ou erro de conta), o ranking top-N por liquidez vira lixo — este
//! teste é a rede de segurança.
use borsh::BorshDeserialize;

fn raw(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).unwrap()
}

#[test]
fn whirlpool_liquidity_offset() {
    let data = raw("wp.raw");
    let pool = orca_whirlpool::WhirlpoolPool::deserialize(&mut &data[8..]).unwrap();
    let off = u128::from_le_bytes(data[49..65].try_into().unwrap());
    assert_eq!(pool.liquidity, off, "Whirlpool liquidity não está em data[49..65]");
}

#[test]
fn clmm_liquidity_offset() {
    let data = raw("clmm.raw");
    let pool = raydium_clmm::RaydiumCLMMPool::deserialize(&mut &data[8..]).unwrap();
    let off = u128::from_le_bytes(data[237..253].try_into().unwrap());
    assert_eq!(pool.liquidity, off, "Raydium CLMM liquidity não está em data[237..253]");
}

#[test]
fn dammv2_liquidity_offset() {
    let data = raw("v2.raw");
    let pool = meteora_damm::MeteoraDAMMV2Pool::deserialize(&mut &data[8..]).unwrap();
    let off = u128::from_le_bytes(data[360..376].try_into().unwrap());
    assert_eq!(pool.liquidity, off, "DAMM V2 liquidity não está em data[360..376]");
}
