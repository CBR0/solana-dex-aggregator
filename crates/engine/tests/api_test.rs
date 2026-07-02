use solana_pubkey::Pubkey;
use thunder_engine::account_store::AccountStore;

#[test]
fn test_account_store_basic() {
    let store = AccountStore::new();
    assert_eq!(store.len(), 0);

    let pk = Pubkey::new_unique();
    store.upsert(pk, vec![0u8; 165], Pubkey::new_unique(), 1000, 100);
    assert_eq!(store.len(), 1);
    assert!(store.contains(&pk));
    assert_eq!(store.last_slot(), 100);
}

#[test]
fn test_account_store_token_balance() {
    let store = AccountStore::new();
    let pk = Pubkey::new_unique();

    // Build a fake SPL token account with balance at bytes 64..72
    let mut data = vec![0u8; 165];
    data[64..72].copy_from_slice(&1_000_000_000u64.to_le_bytes());

    store.upsert(pk, data, Pubkey::new_unique(), 2_039_280, 100);
    assert_eq!(store.read_token_balance(&pk), 1_000_000_000);
}

#[test]
fn test_account_store_token_balance_short_data() {
    let store = AccountStore::new();
    let pk = Pubkey::new_unique();

    // Data shorter than 72 bytes — read_token_balance should return 0
    store.upsert(pk, vec![0u8; 32], Pubkey::new_unique(), 0, 1);
    assert_eq!(store.read_token_balance(&pk), 0);
}

#[test]
fn test_account_store_upsert_updates_slot() {
    let store = AccountStore::new();
    let pk = Pubkey::new_unique();

    store.upsert(pk, vec![1], Pubkey::new_unique(), 0, 50);
    assert_eq!(store.last_slot(), 50);

    store.upsert(pk, vec![2], Pubkey::new_unique(), 0, 100);
    assert_eq!(store.last_slot(), 100);
    // Count shouldn't increase for same key
    assert_eq!(store.len(), 1);
}

#[test]
fn test_account_store_get_data() {
    let store = AccountStore::new();
    let pk = Pubkey::new_unique();
    let data = vec![42u8; 80];

    store.upsert(pk, data.clone(), Pubkey::new_unique(), 500, 10);
    assert_eq!(store.get_data(&pk), Some(data));

    let missing = Pubkey::new_unique();
    assert_eq!(store.get_data(&missing), None);
}

#[test]
fn test_account_store_contains_missing() {
    let store = AccountStore::new();
    assert!(!store.contains(&Pubkey::new_unique()));
}

#[test]
fn test_account_store_multiple_keys() {
    let store = AccountStore::new();
    let keys: Vec<Pubkey> = (0..10).map(|_| Pubkey::new_unique()).collect();

    for (i, pk) in keys.iter().enumerate() {
        store.upsert(*pk, vec![i as u8], Pubkey::new_unique(), 0, i as u64);
    }

    assert_eq!(store.len(), 10);
    assert_eq!(store.last_slot(), 9);

    for pk in &keys {
        assert!(store.contains(pk));
    }
}
