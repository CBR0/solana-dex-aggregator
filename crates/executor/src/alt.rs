//! Address Lookup Table helpers.
//!
//! A v0 transaction can resolve accounts through an on-chain lookup table,
//! referencing each by a 1-byte index instead of a 32-byte key. That is what
//! lets multi-hop / multi-protocol routes fit under the 1232-byte tx limit.
//!
//! [`collect_addresses`] gathers the accounts a route touches (to put in a
//! table). Creating the table on-chain (`create_lookup_table` + `extend`) is a
//! signed, multi-tx setup step — a follow-up; this module provides the pure
//! collection + the local `AddressLookupTableAccount` assembly used for sizing.

use std::collections::HashSet;

use solana_address_lookup_table_interface::instruction::{
    create_lookup_table, extend_lookup_table,
};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::AddressLookupTableAccount;
use solana_sdk::signature::Signer;

use thunder_core::GenericError;

use crate::submit;

/// Addresses added per extend transaction (keeps each extend tx well under the
/// size limit; each address is 32 bytes).
const EXTEND_CHUNK: usize = 20;

/// Unique accounts + program ids referenced across the instructions, excluding
/// `exclude` (pass the fee payer — signers must stay static, not in a table).
/// Insertion order preserved for stable indices.
pub fn collect_addresses(instructions: &[Instruction], exclude: &[Pubkey]) -> Vec<Pubkey> {
    let skip: HashSet<Pubkey> = exclude.iter().copied().collect();
    let mut seen: HashSet<Pubkey> = skip.clone();
    let mut out: Vec<Pubkey> = Vec::new();
    for ix in instructions {
        if seen.insert(ix.program_id) {
            out.push(ix.program_id);
        }
        for meta in &ix.accounts {
            if seen.insert(meta.pubkey) {
                out.push(meta.pubkey);
            }
        }
    }
    out
}

/// Assemble a local `AddressLookupTableAccount` from a table key + addresses,
/// for compiling/sizing a v0 message. (The table must actually exist on-chain
/// before such a transaction can be sent.)
pub fn lookup_table(key: Pubkey, addresses: Vec<Pubkey>) -> AddressLookupTableAccount {
    AddressLookupTableAccount { key, addresses }
}

/// Load an existing on-chain lookup table into an `AddressLookupTableAccount`.
/// Use this to reuse a pre-warmed table instead of creating one per trade.
pub async fn fetch_lookup_table(
    rpc: &RpcClient,
    key: Pubkey,
) -> Result<AddressLookupTableAccount, GenericError> {
    let data = rpc.get_account_data(&key).await?;
    let table = AddressLookupTable::deserialize(&data)
        .map_err(|e| GenericError::from(format!("decode lookup table: {e}")))?;
    Ok(AddressLookupTableAccount { key, addresses: table.addresses.to_vec() })
}

/// Create a lookup table on-chain, extend it with `addresses`, and wait until it
/// is active (usable one slot after the last extend). Signs and sends the setup
/// transactions with `payer`. Returns the ready-to-use table.
///
/// This is the on-the-fly path — it adds ~2+ setup transactions plus a slot of
/// latency. For hot paths, pre-warm a table once and reuse it via
/// [`fetch_lookup_table`].
pub async fn create_and_extend_lookup_table(
    rpc: &RpcClient,
    payer: &dyn Signer,
    addresses: &[Pubkey],
) -> Result<AddressLookupTableAccount, GenericError> {
    let authority = payer.pubkey();
    // The ALT program derives the table address from a slot that must be present
    // in SlotHashes when the tx executes. Use the finalized slot and back off a
    // margin so it's comfortably inside the recent-slot window (avoids the race
    // where a just-finalized slot isn't yet in the processing node's SlotHashes).
    let finalized = rpc
        .get_slot_with_commitment(solana_commitment_config::CommitmentConfig::finalized())
        .await?;
    let recent_slot = finalized.saturating_sub(20);
    let (create_ix, table) = create_lookup_table(authority, authority, recent_slot);

    // Create the (empty) table.
    let blockhash = rpc.get_latest_blockhash().await?;
    let tx = submit::build_signed_transaction(payer, &[create_ix], blockhash)?;
    rpc.send_and_confirm_transaction(&tx).await?;

    // Extend in chunks so each extend tx stays small.
    for chunk in addresses.chunks(EXTEND_CHUNK) {
        let ix = extend_lookup_table(table, authority, Some(authority), chunk.to_vec());
        let blockhash = rpc.get_latest_blockhash().await?;
        let tx = submit::build_signed_transaction(payer, &[ix], blockhash)?;
        rpc.send_and_confirm_transaction(&tx).await?;
    }

    // A table is usable one slot after its last modification — wait for the
    // slot to advance (bounded poll).
    let extended_at = rpc.get_slot().await?;
    for _ in 0..40 {
        if rpc.get_slot().await? > extended_at {
            break;
        }
    }

    Ok(AddressLookupTableAccount { key: table, addresses: addresses.to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::AccountMeta;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    #[test]
    fn collects_unique_accounts_and_programs_excluding_payer() {
        let ix1 = Instruction::new_with_bytes(
            pk(100),
            &[],
            vec![AccountMeta::new(pk(1), true), AccountMeta::new_readonly(pk(2), false)],
        );
        let ix2 = Instruction::new_with_bytes(
            pk(100), // same program → deduped
            &[],
            vec![AccountMeta::new(pk(2), false), AccountMeta::new(pk(3), false)],
        );
        let addrs = collect_addresses(&[ix1, ix2], &[pk(1)]); // exclude payer pk(1)
        assert!(!addrs.contains(&pk(1)));
        assert!(addrs.contains(&pk(100)));
        assert!(addrs.contains(&pk(2)));
        assert!(addrs.contains(&pk(3)));
        // program(100) + acct(2) + acct(3) = 3 unique
        assert_eq!(addrs.len(), 3);
    }
}
