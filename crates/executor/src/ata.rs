//! Associated-token-account helpers and native-SOL wrapping.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::Instruction;
use solana_system_interface::instruction as system_instruction;

use solroute_core::{TOKEN_PROGRAM, WSOL};

/// WSOL mint.
pub fn wsol() -> Pubkey {
    Pubkey::from_str_const(WSOL)
}

/// SPL Token program.
pub fn token_program() -> Pubkey {
    Pubkey::from_str_const(TOKEN_PROGRAM)
}

/// Associated token account for `(owner, mint)` under a specific token program.
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address_with_program_id(
        owner,
        mint,
        token_program,
    )
}

/// Idempotent ATA creation (no-op on-chain if it already exists).
pub fn create_ata_idempotent(
    funder: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        funder,
        owner,
        mint,
        token_program,
    )
}

/// Wrap `lamports` of native SOL into the payer's WSOL ATA:
/// create (idempotent) → fund → `sync_native`.
pub fn wrap_sol_ixs(payer: &Pubkey, lamports: u64) -> Vec<Instruction> {
    let tp = token_program();
    let w = wsol();
    let ata_addr = ata(payer, &w, &tp);
    vec![
        create_ata_idempotent(payer, payer, &w, &tp),
        system_instruction::transfer(payer, &ata_addr, lamports),
        spl_token::instruction::sync_native(&tp, &ata_addr).expect("sync_native ix"),
    ]
}

/// Close a token account, sending rent + any balance to `owner`.
pub fn close_account_ix(token_program: &Pubkey, account: &Pubkey, owner: &Pubkey) -> Instruction {
    spl_token::instruction::close_account(token_program, account, owner, owner, &[owner])
        .expect("close_account ix")
}
