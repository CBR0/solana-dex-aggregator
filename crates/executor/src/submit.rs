//! Transaction assembly and submission.
//!
//! Minimal path: prepend compute-budget instructions, build a signed legacy
//! transaction, send via RPC. No SWQoS/Jito fan-out (kept out of scope for the
//! selective port); callers wanting MEV lanes can serialize the signed tx and
//! forward it themselves.

use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_rpc_client_api::response::RpcSimulateTransactionResult;
use solana_hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_message::{v0, AddressLookupTableAccount, Message, VersionedMessage};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::{versioned::VersionedTransaction, Transaction};

use solroute_core::GenericError;

/// Compute Budget program.
const COMPUTE_BUDGET_PROGRAM: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// Compute-budget instructions: set the CU limit and the priority price
/// (micro-lamports per CU). Built from the on-chain wire format directly
/// (`SetComputeUnitLimit` = 2, `SetComputeUnitPrice` = 3) to avoid a
/// version-fragile dependency.
pub fn compute_budget_ixs(unit_limit: u32, unit_price_micro_lamports: u64) -> Vec<Instruction> {
    let mut limit_data = Vec::with_capacity(5);
    limit_data.push(2u8);
    limit_data.extend_from_slice(&unit_limit.to_le_bytes());

    let mut price_data = Vec::with_capacity(9);
    price_data.push(3u8);
    price_data.extend_from_slice(&unit_price_micro_lamports.to_le_bytes());

    vec![
        Instruction::new_with_bytes(COMPUTE_BUDGET_PROGRAM, &limit_data, vec![]),
        Instruction::new_with_bytes(COMPUTE_BUDGET_PROGRAM, &price_data, vec![]),
    ]
}

/// Build a signed legacy transaction from instructions and a fresh blockhash.
pub fn build_signed_transaction(
    payer: &dyn Signer,
    instructions: &[Instruction],
    recent_blockhash: Hash,
) -> Result<Transaction, GenericError> {
    let message = Message::new_with_blockhash(instructions, Some(&payer.pubkey()), &recent_blockhash);
    let mut tx = Transaction::new_unsigned(message);
    tx.try_sign(&[payer], recent_blockhash)?;
    Ok(tx)
}

/// Send without waiting for confirmation (fast path).
pub async fn send(rpc: &RpcClient, tx: &Transaction) -> Result<Signature, GenericError> {
    Ok(rpc.send_transaction(tx).await?)
}

/// Send and confirm.
pub async fn send_and_confirm(rpc: &RpcClient, tx: &Transaction) -> Result<Signature, GenericError> {
    Ok(rpc.send_and_confirm_transaction(tx).await?)
}

/// Build an unsigned transaction (fee payer set, placeholder signatures). Use
/// with [`simulate`] — no keypair required.
pub fn build_unsigned_transaction(payer: &Pubkey, instructions: &[Instruction]) -> Transaction {
    let message = Message::new_with_blockhash(instructions, Some(payer), &Hash::default());
    Transaction::new_unsigned(message)
}

// --- v0 (versioned) transactions + Address Lookup Tables ---------------------

/// Compile a v0 message that resolves writable/readonly accounts through the
/// given lookup tables (1-byte index instead of a 32-byte key each). Pass an
/// empty slice for no ALTs.
pub fn compile_v0_message(
    payer: &Pubkey,
    instructions: &[Instruction],
    lookup_tables: &[AddressLookupTableAccount],
    recent_blockhash: Hash,
) -> Result<VersionedMessage, GenericError> {
    let msg = v0::Message::try_compile(payer, instructions, lookup_tables, recent_blockhash)?;
    Ok(VersionedMessage::V0(msg))
}

/// Build a signed v0 transaction using lookup tables.
pub fn build_signed_v0_transaction(
    payer: &dyn Signer,
    instructions: &[Instruction],
    lookup_tables: &[AddressLookupTableAccount],
    recent_blockhash: Hash,
) -> Result<VersionedTransaction, GenericError> {
    let message = compile_v0_message(&payer.pubkey(), instructions, lookup_tables, recent_blockhash)?;
    Ok(VersionedTransaction::try_new(message, &[payer])?)
}

/// Build an unsigned v0 transaction (placeholder signatures) for simulation.
pub fn build_unsigned_v0_transaction(
    payer: &Pubkey,
    instructions: &[Instruction],
    lookup_tables: &[AddressLookupTableAccount],
) -> Result<VersionedTransaction, GenericError> {
    let message = compile_v0_message(payer, instructions, lookup_tables, Hash::default())?;
    let num_sigs = message.header().num_required_signatures as usize;
    Ok(VersionedTransaction { signatures: vec![Signature::default(); num_sigs], message })
}

/// Wire size of a versioned transaction in bytes. The network cap is 1232;
/// use this to decide whether a route fits (and whether ALTs are needed).
pub fn versioned_tx_size(tx: &VersionedTransaction) -> usize {
    bincode::serialize(tx).map(|b| b.len()).unwrap_or(usize::MAX)
}

/// Send a versioned transaction (no wait).
pub async fn send_versioned(
    rpc: &RpcClient,
    tx: &VersionedTransaction,
) -> Result<Signature, GenericError> {
    Ok(rpc.send_transaction(tx).await?)
}

/// Maximum transaction wire size (bytes).
pub const TX_SIZE_LIMIT: usize = 1232;

/// Simulate a versioned transaction without landing it.
pub async fn simulate_versioned(
    rpc: &RpcClient,
    tx: &VersionedTransaction,
) -> Result<RpcSimulateTransactionResult, GenericError> {
    let config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        ..Default::default()
    };
    Ok(rpc.simulate_transaction_with_config(tx, config).await?.value)
}

/// Simulate a transaction without landing it. Skips signature verification and
/// lets the RPC substitute a fresh blockhash, so an unsigned tx works. Returns
/// the program logs, error (if any), and units consumed.
pub async fn simulate(
    rpc: &RpcClient,
    tx: &Transaction,
) -> Result<RpcSimulateTransactionResult, GenericError> {
    let config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        ..Default::default()
    };
    Ok(rpc.simulate_transaction_with_config(tx, config).await?.value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_keypair::Keypair;

    #[test]
    fn compute_budget_wire_format() {
        let ixs = compute_budget_ixs(200_000, 1_000);
        assert_eq!(ixs[0].data[0], 2); // SetComputeUnitLimit
        assert_eq!(u32::from_le_bytes(ixs[0].data[1..5].try_into().unwrap()), 200_000);
        assert_eq!(ixs[1].data[0], 3); // SetComputeUnitPrice
        assert_eq!(u64::from_le_bytes(ixs[1].data[1..9].try_into().unwrap()), 1_000);
    }

    #[test]
    fn signs_a_transaction_offline() {
        let payer = Keypair::new();
        let ixs = compute_budget_ixs(200_000, 1_000);
        let tx = build_signed_transaction(&payer, &ixs, Hash::default()).unwrap();
        assert_eq!(tx.signatures.len(), 1);
        assert!(tx.is_signed());
    }
}
