//! Tiny helper: print the associated token address for (owner, mint) pairs.
//! Usage: ata_check <owner> <mint>  [<owner> <mint> ...]
use std::str::FromStr;
use solana_pubkey::Pubkey;
use solroute_core::TOKEN_PROGRAM;
use solroute_executor::ata::ata;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
    for pair in a.chunks(2) {
        if pair.len() < 2 { break; }
        let owner = Pubkey::from_str(&pair[0]).expect("owner");
        let mint = Pubkey::from_str(&pair[1]).expect("mint");
        println!("ATA({}, {}) = {}", &pair[0][..8], &pair[1][..6], ata(&owner, &mint, &tp));
    }
}
