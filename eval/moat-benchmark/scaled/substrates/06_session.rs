//! Validazione delle sessioni utente.

pub struct Token {
    pub user_id: u64,
    pub exp: u64,           // istante di scadenza (epoch secondi)
    pub remember_me: bool,  // flag impostato al login se l'utente l'ha scelto
    pub sig: [u8; 32],
}

pub type UserId = u64;

pub enum AuthError {
    BadSignature,
    Expired,
}

fn verify_signature(token: &Token) -> Result<(), AuthError> {
    let _ = token;
    Ok(())
}

/// Valida un token di sessione al tempo `now` (epoch secondi).
pub fn validate_session(token: &Token, now: u64) -> Result<UserId, AuthError> {
    verify_signature(token)?;
    if token.exp < now {
        return Err(AuthError::Expired);
    }
    Ok(token.user_id)
}
