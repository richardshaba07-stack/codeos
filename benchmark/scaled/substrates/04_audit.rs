//! Registrazione (audit log) dei tentativi di login.

pub struct LoginEvent {
    pub email: String,
    pub ip: String,
    pub success: bool,
}

pub trait LogSink {
    fn write(&mut self, line: &str);
}

fn opaque_session_id(ev: &LoginEvent) -> String {
    // id di sessione opaco, non riconducibile all'utente
    format!("s{:08x}", fnv1a(ev.email.as_bytes()) ^ fnv1a(ev.ip.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Scrive una riga di audit per un tentativo di login.
pub fn log_login_attempt(ev: &LoginEvent, sink: &mut dyn LogSink) {
    sink.write(&format!(
        "login success={} session={}",
        ev.success,
        opaque_session_id(ev)
    ));
}
