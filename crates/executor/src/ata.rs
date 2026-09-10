//! Associated-token-account helpers and native-SOL wrapping.
//!
//! The SPL `spl-token` / `spl-associated-token-account` crates are pinned to
//! the pre-v1 Solana SDK major and cannot compile in a 4.x-family tree, so
//! the handful of instructions used here are built directly from their
//! consensus-frozen layouts (verified against
//! `spl-token-interface@3.0.0` / `spl-associated-token-account-interface@2.0.0`):
//! - ATA `CreateIdempotent`: data `[1]`; accounts
//!   `[funder(w,signer), ata(w), wallet(r), mint(r), system(r), token_program(r)]`
//! - SPL `SyncNative`: discriminant `17`; accounts `[account(w)]`
//! - SPL `CloseAccount`: discriminant `9`; accounts
//!   `[account(w), destination(w), owner]` (+ multisig signers, unused here)

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_system_interface::instruction as system_instruction;

use solroute_core::{TOKEN_PROGRAM, WSOL};

/// Associated Token Account program.
pub fn ata_program() -> Pubkey {
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
}

/// System program.
fn system_program() -> Pubkey {
    Pubkey::from_str_const("11111111111111111111111111111111")
}

/// WSOL mint.
pub fn wsol() -> Pubkey {
    Pubkey::from_str_const(WSOL)
}

/// SPL Token program.
pub fn token_program() -> Pubkey {
    Pubkey::from_str_const(TOKEN_PROGRAM)
}

/// Associated token account for `(owner, mint)` under a specific token program.
/// Seeds: `[wallet, token_program, mint]` under the ATA program id.
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            &owner.to_bytes(),
            &token_program.to_bytes(),
            &mint.to_bytes(),
        ],
        &ata_program(),
    )
    .0
}

/// Idempotent ATA creation (no-op on-chain if it already exists).
pub fn create_ata_idempotent(
    funder: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ata_program(),
        accounts: vec![
            AccountMeta::new(*funder, true),
            AccountMeta::new(ata(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// SPL `SyncNative` instruction (discriminant 17).
fn sync_native_ix(token_program: &Pubkey, account: &Pubkey) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![AccountMeta::new(*account, false)],
        data: vec![17],
    }
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
        sync_native_ix(&tp, &ata_addr),
    ]
}

/// Close a token account, sending rent + any balance to `owner`.
/// Byte-for-byte the layout `spl_token::instruction::close_account(
/// token_program, account, owner, owner, &[owner])` produces: the destination
/// is `owner` itself and `owner` is passed both as the (non-signer) authority
/// slot and as the signer slot.
pub fn close_account_ix(token_program: &Pubkey, account: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*owner, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9],
    }
}
