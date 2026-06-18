//! Substrato del Task B del benchmark del moat: una policy di pagamento NON
//! derivabile dal codice (il «perché» del no-retry non è scritto qui).
//!
//! Decisione (non nel codice, solo nel ledger): «MAI retry automatico dentro
//! process_payment su errore di rete — un timeout NON significa addebito
//! fallito (il gateway può aver già addebitato e perso la risposta) → retry =
//! DOPPIO ADDEBITO. I retry sono sicuri solo a un livello che riusa la stessa
//! idempotency_key e verifica lo stato prima di ri-addebitare.»

pub struct PaymentRequest {
    pub user_id: u64,
    pub amount_cents: u64,
    pub idempotency_key: String,
}

pub enum PaymentError {
    Network,
    Declined,
    Invalid,
}

/// Invia la richiesta al gateway di pagamento e ritorna l'esito.
pub fn process_payment(req: &PaymentRequest, gateway: &Gateway) -> Result<Receipt, PaymentError> {
    let resp = gateway.charge(req.user_id, req.amount_cents, &req.idempotency_key)?;
    Ok(Receipt::from(resp))
}
