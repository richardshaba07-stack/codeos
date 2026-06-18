//! User session validation.

pub struct Token {
    pub user_id: u64,
    pub exp: u64,           // expiry instant (epoch seconds)
    pub remember_me: bool,  // flag set at login if the user chose it
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

/// Validates a session token at time `now` (epoch seconds).
pub fn validate_session(token: &Token, now: u64) -> Result<UserId, AuthError> {
    verify_signature(token)?;
    if token.exp < now {
        return Err(AuthError::Expired);
    }
    Ok(token.user_id)
}
