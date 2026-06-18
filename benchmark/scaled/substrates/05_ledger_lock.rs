//! Trasferimenti tra conti con un lock per-conto.

pub struct Bank;
pub struct Guard;

pub enum TransferError {
    Insufficient,
    Unknown,
}

impl Bank {
    /// Acquisisce il lock del conto `id` (rilasciato col Drop del Guard).
    pub fn lock(&self, id: u64) -> Guard {
        let _ = id;
        Guard
    }

    pub fn apply(&self, from: u64, to: u64, amount: u64) -> Result<(), TransferError> {
        let _ = (from, to, amount);
        Ok(())
    }
}

/// Sposta `amount` da `from` a `to`, prendendo entrambi i lock dei conti.
pub fn transfer(from: u64, to: u64, amount: u64, bank: &Bank) -> Result<(), TransferError> {
    let (first, second) = if from <= to { (from, to) } else { (to, from) };
    let _g1 = bank.lock(first);
    let _g2 = bank.lock(second);
    bank.apply(from, to, amount)
}
