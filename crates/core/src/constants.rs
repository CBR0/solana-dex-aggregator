//! Well-known addresses shared across multiple DEX implementations.

use solana_pubkey::Pubkey;

// Wrapped SOL
pub const WSOL: &str = "So11111111111111111111111111111111111111112";

// Stablecoins
pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
pub const PYUSD: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";

// Liquid staking tokens (SOL-denominated)
pub const JITOSOL: &str = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn";
pub const MSOL: &str = "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So";
pub const BSOL: &str = "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1";
pub const JUPSOL: &str = "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v";

// Token programs
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_PROGRAM_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const MEMO_PROGRAM_V2: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// All mints considered "quote currencies" for normalization.
/// Order matters: earlier entries are preferred when both sides qualify.
const QUOTE_MINTS: &[&str] = &[WSOL, USDC, USDT, PYUSD, JITOSOL, MSOL, BSOL, JUPSOL];

/// Returns true if the given mint is a well-known quote currency.
///
/// Used by DEX crates to normalize quote/base ordering when the on-chain pool
/// stores mints in an arbitrary order. Covers SOL, stablecoins, and major
/// liquid staking tokens.
pub fn is_quote_mint(mint: &Pubkey) -> bool {
    QUOTE_MINTS
        .iter()
        .any(|&q| *mint == Pubkey::from_str_const(q))
}

/// Returns a priority rank for quote currency ordering (lower = more preferred).
/// WSOL is most preferred (0), then USDC (1), etc. Returns `None` if not a quote mint.
///
/// When both sides of a pool are quote currencies (e.g., USDC/WSOL), the one with
/// the lower rank becomes the quote side.
pub fn quote_priority(mint: &Pubkey) -> Option<usize> {
    QUOTE_MINTS
        .iter()
        .position(|&q| *mint == Pubkey::from_str_const(q))
}


/// Infer token decimals from the mint address.
///
/// Uses a static lookup for well-known tokens, heuristic detection for
/// pump.fun tokens (address ending in `pump` → always 6 decimals),
/// and defaults to 9 for everything else.
///
/// For production accuracy, callers should fetch the actual SPL Mint account
/// and read byte 44 (the decimals field). This function is a best-effort
/// fallback that covers >95% of Solana tokens correctly.
pub fn infer_mint_decimals(mint: &Pubkey) -> u8 {
    // Well-known tokens with non-9 decimals.
    const KNOWN: &[(&str, u8)] = &[
        // 6-decimal stablecoins
        ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6), // USDC
        ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", 6), // USDT
        ("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", 6), // PYUSD
        ("USD1ttGY1N17NEEHLmELoaybftRBUSErhqYiQzvEmuB", 6),  // USD1
        // 8-decimal bridged assets
        ("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs", 8), // WETH (Wormhole)
        ("3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh", 8), // cbBTC
        ("HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr", 8), // WBTC (Wormhole)
        // 6-decimal DeFi tokens
        ("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", 6), // WIF
        ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5), // BONK
    ];

    for &(addr, dec) in KNOWN {
        if *mint == Pubkey::from_str_const(addr) {
            return dec;
        }
    }

    // pump.fun tokens always have 6 decimals.
    let s = mint.to_string();
    if s.ends_with("pump") {
        return 6;
    }

    // Default: 9 decimals (SOL and vast majority of SPL tokens).
    9
}