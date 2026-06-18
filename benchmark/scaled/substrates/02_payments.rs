//! Payment module.

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

/// Sends the request to the payment gateway and returns the outcome.
pub fn process_payment(req: &PaymentRequest, gateway: &Gateway) -> Result<Receipt, PaymentError> {
    let resp = gateway.charge(req.user_id, req.amount_cents, &req.idempotency_key)?;
    Ok(Receipt::from(resp))
}
