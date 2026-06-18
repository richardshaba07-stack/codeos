//! Transfers between accounts with a per-account lock.

pub struct Bank;
pub struct Guard;

pub enum TransferError {
    Insufficient,
    Unknown,
}

impl Bank {
    /// Acquires the lock for account `id` (released when the Guard is dropped).
    pub fn lock(&self, id: u64) -> Guard {
        let _ = id;
        Guard
    }

    pub fn apply(&self, from: u64, to: u64, amount: u64) -> Result<(), TransferError> {
        let _ = (from, to, amount);
        Ok(())
    }
}

/// Moves `amount` from `from` to `to`, taking both account locks.
pub fn transfer(from: u64, to: u64, amount: u64, bank: &Bank) -> Result<(), TransferError> {
    let (first, second) = if from <= to { (from, to) } else { (to, from) };
    let _g1 = bank.lock(first);
    let _g2 = bank.lock(second);
    bank.apply(from, to, amount)
}
