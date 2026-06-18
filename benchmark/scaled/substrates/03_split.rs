//! Splitting a bill among N people.

/// Splits `total_cents` among `n` people. Returns the shares as whole cents;
/// the division remainder is distributed one unit at a time.
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

/// Summary row shown in the UI for one person.
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
