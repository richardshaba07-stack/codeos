//! Suddivisione di un conto tra N persone.

/// Divide `total_cents` tra `n` persone. Ritorna le quote in centesimi interi;
/// il resto della divisione viene distribuito una unità alla volta.
pub fn split_bill(total_cents: u64, n: u32) -> Vec<u64> {
    let n = n as u64;
    let base = total_cents / n;
    let remainder = total_cents % n;
    let mut shares = vec![base; n as usize];
    for share in shares.iter_mut().take(remainder as usize) {
        *share += 1;
    }
    shares
}

/// Riga di riepilogo mostrata nell'UI per una persona.
pub struct ShareRow {
    pub person: String,
    pub share_cents: u64,
}

pub fn render_shares(people: &[String], total_cents: u64) -> Vec<ShareRow> {
    let shares = split_bill(total_cents, people.len() as u32);
    people
        .iter()
        .zip(shares)
        .map(|(person, share_cents)| ShareRow {
            person: person.clone(),
            share_cents,
        })
        .collect()
}
