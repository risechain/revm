use super::{
    plain_account::PlainStorage, transition_account::TransitionAccount, CacheAccount, PlainAccount,
};
use core::hash::{BuildHasherDefault, Hasher};
use dashmap::DashMap;
use revm_interpreter::primitives::{
    Account, AccountInfo, Address, Bytecode, EvmState, HashMap, B256,
};
use std::vec::Vec;

/// We use the last 8 bytes of an existing hash like address
/// or code hash instead of rehashing it.
// TODO: Make sure this is acceptable for production
#[derive(Debug, Default)]
pub struct SuffixHasher(u64);
impl Hasher for SuffixHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut suffix = [0u8; 8];
        suffix.copy_from_slice(&bytes[bytes.len() - 8..]);
        self.0 = u64::from_be_bytes(suffix);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
/// Build a suffix hasher
pub type BuildSuffixHasher = BuildHasherDefault<SuffixHasher>;

/// Cache state contains both modified and original values.
///
/// Cache state is main state that revm uses to access state.
/// It loads all accounts from database and applies revm output to it.
///
/// It generates transitions that is used to build BundleState.
#[derive(Clone, Debug)]
pub struct CacheState {
    /// Block state account with account state.
    pub accounts: DashMap<Address, CacheAccount, BuildSuffixHasher>,
    /// Created contracts.
    // TODO add bytecode counter for number of bytecodes added/removed.
    pub contracts: DashMap<B256, Bytecode, BuildSuffixHasher>,
    /// Has EIP-161 state clear enabled (Spurious Dragon hardfork).
    pub has_state_clear: bool,
}

impl Default for CacheState {
    fn default() -> Self {
        Self::new(true)
    }
}

impl CacheState {
    /// New default state.
    pub fn new(has_state_clear: bool) -> Self {
        Self {
            accounts: DashMap::default(),
            contracts: DashMap::default(),
            has_state_clear,
        }
    }

    /// Set state clear flag. EIP-161.
    pub fn set_state_clear_flag(&mut self, has_state_clear: bool) {
        self.has_state_clear = has_state_clear;
    }

    /// Helper function that returns all accounts.
    ///
    /// Used inside tests to generate merkle tree.
    pub fn trie_account(&self) -> impl IntoIterator<Item = (Address, PlainAccount)> + '_ {
        self.accounts.iter().filter_map(|r| {
            r.value()
                .account
                .as_ref()
                .map(|plain_acc| (*r.key(), plain_acc.clone()))
        })
    }

    /// Insert not existing account.
    pub fn insert_not_existing(&mut self, address: Address) {
        self.accounts
            .insert(address, CacheAccount::new_loaded_not_existing());
    }

    /// Insert Loaded (Or LoadedEmptyEip161 if account is empty) account.
    pub fn insert_account(&mut self, address: Address, info: AccountInfo) {
        let account = if !info.is_empty() {
            CacheAccount::new_loaded(info, HashMap::default())
        } else {
            CacheAccount::new_loaded_empty_eip161(HashMap::default())
        };
        self.accounts.insert(address, account);
    }

    /// Similar to `insert_account` but with storage.
    pub fn insert_account_with_storage(
        &mut self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        let account = if !info.is_empty() {
            CacheAccount::new_loaded(info, storage)
        } else {
            CacheAccount::new_loaded_empty_eip161(storage)
        };
        self.accounts.insert(address, account);
    }

    /// Apply output of revm execution and create account transitions that are used to build BundleState.
    pub fn apply_evm_state(&mut self, evm_state: EvmState) -> Vec<(Address, TransitionAccount)> {
        let mut transitions = Vec::with_capacity(evm_state.len());
        for (address, account) in evm_state {
            if let Some(transition) = self.apply_account_state(address, account) {
                transitions.push((address, transition));
            }
        }
        transitions
    }

    /// Apply updated account state to the cached account.
    /// Returns account transition if applicable.
    fn apply_account_state(&self, address: Address, account: Account) -> Option<TransitionAccount> {
        // not touched account are never changed.
        if !account.is_touched() {
            return None;
        }

        let mut this_account = self
            .accounts
            .get_mut(&address)
            .expect("All accounts should be present inside cache");

        // If it is marked as selfdestructed inside revm
        // we need to changed state to destroyed.
        if account.is_selfdestructed() {
            return this_account.selfdestruct();
        }

        let is_created = account.is_created();
        let is_empty = account.is_empty();

        // transform evm storage to storage with previous value.
        let changed_storage = account
            .storage
            .into_iter()
            .filter(|(_, slot)| slot.is_changed())
            .map(|(key, slot)| (key, slot.into()))
            .collect();

        // Note: it can happen that created contract get selfdestructed in same block
        // that is why is_created is checked after selfdestructed
        //
        // Note: Create2 opcode (Petersburg) was after state clear EIP (Spurious Dragon)
        //
        // Note: It is possibility to create KECCAK_EMPTY contract with some storage
        // by just setting storage inside CRATE constructor. Overlap of those contracts
        // is not possible because CREATE2 is introduced later.
        if is_created {
            self.contracts
                .entry(account.info.code_hash)
                .or_insert_with(|| account.info.code.clone().unwrap());
            return Some(this_account.newly_created(account.info, changed_storage));
        }

        // Account is touched, but not selfdestructed or newly created.
        // Account can be touched and not changed.
        // And when empty account is touched it needs to be removed from database.
        // EIP-161 state clear
        if is_empty {
            if self.has_state_clear {
                // touch empty account.
                this_account.touch_empty_eip161()
            } else {
                // if account is empty and state clear is not enabled we should save
                // empty account.
                this_account.touch_create_pre_eip161(changed_storage)
            }
        } else {
            Some(this_account.change(account.info, changed_storage))
        }
    }
}
