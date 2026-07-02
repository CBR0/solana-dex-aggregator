use dashmap::DashMap;
use solana_pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, Ordering};

use thunder_core::AccountDataProvider;

pub struct AccountData {
    pub data: Vec<u8>,
    pub owner: Pubkey,
    pub lamports: u64,
    pub slot: u64,
}

pub struct AccountStore {
    accounts: DashMap<Pubkey, AccountData>,
    last_slot: AtomicU64,
    account_count: AtomicU64,
}

impl AccountStore {
    pub fn new() -> Self {
        Self {
            accounts: DashMap::new(),
            last_slot: AtomicU64::new(0),
            account_count: AtomicU64::new(0),
        }
    }

    pub fn upsert(&self, pubkey: Pubkey, data: Vec<u8>, owner: Pubkey, lamports: u64, slot: u64) {
        let is_new = !self.accounts.contains_key(&pubkey);
        self.accounts.insert(
            pubkey,
            AccountData {
                data,
                owner,
                lamports,
                slot,
            },
        );
        if is_new {
            self.account_count.fetch_add(1, Ordering::Relaxed);
        }
        let prev = self.last_slot.load(Ordering::Relaxed);
        if slot > prev {
            self.last_slot.store(slot, Ordering::Relaxed);
        }
    }

    pub fn get_data(&self, pubkey: &Pubkey) -> Option<Vec<u8>> {
        self.accounts.get(pubkey).map(|v| v.data.clone())
    }

    pub fn get(&self, pubkey: &Pubkey) -> Option<dashmap::mapref::one::Ref<'_, Pubkey, AccountData>> {
        self.accounts.get(pubkey)
    }

    pub fn contains(&self, pubkey: &Pubkey) -> bool {
        self.accounts.contains_key(pubkey)
    }

    pub fn read_token_balance(&self, pubkey: &Pubkey) -> u64 {
        self.accounts
            .get(pubkey)
            .filter(|v| v.data.len() >= 72)
            .map(|v| u64::from_le_bytes(v.data[64..72].try_into().unwrap()))
            .unwrap_or(0)
    }

    pub fn last_slot(&self) -> u64 {
        self.last_slot.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> u64 {
        self.account_count.load(Ordering::Relaxed)
    }
}

impl AccountDataProvider for AccountStore {
    fn pool_account_data(&self, pubkey: &Pubkey) -> Option<Vec<u8>> {
        self.get_data(pubkey)
    }

    fn token_balance(&self, vault_pubkey: &Pubkey) -> u64 {
        self.read_token_balance(vault_pubkey)
    }
}
