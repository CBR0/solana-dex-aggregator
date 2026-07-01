use solana_pubkey::Pubkey;

use crate::METEORA_VAULT_PROGRAM;

pub static TOKEN_VAULT_PREFIX: &str = "token_vault";

pub fn derive_token_vault_address(vault: Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[TOKEN_VAULT_PREFIX.as_ref(), vault.as_ref()],
        &Pubkey::from_str_const(METEORA_VAULT_PROGRAM),
    )
}