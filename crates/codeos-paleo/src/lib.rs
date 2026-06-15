//! `codeos-paleo` — il **Paleontologo**: legge lo *spazio negativo del tempo*.
//!
//! # L'idea (mai vista in nessuno strumento)
//!
//! Il Guardian scopre invarianti dallo spazio negativo della **struttura**: se il
//! grafo ha molti archi `A → B` e **zero** `B → A`, allora `B → A` è proibito.
//! Ma quella confidenza è strutturale e statica: non distingue un invariante
//! sopravvissuto a mille occasioni di essere violato da una semplice coincidenza
//! di un grafo giovane.
//!
//! Questo crate proietta la stessa intuizione su un asse nuovo — il **tempo**.
//! Definisce un osservabile che nessun tool misura: l'**evento di astensione**.
//!
//! > Un *occasione* è un commit che ha toccato **sia** il layer A **sia** il layer
//! > B: in quel commit lo sviluppatore *avrebbe potuto* cablare l'arco proibito
//! > `B → A`. Se non l'ha fatto, è un'**astensione**: una micro-conferma rivelata
//! > dal comportamento del team, non da un commento o da una regola scritta.
//!
//! La confidenza di un invariante diventa allora il **lower bound di Wilson** sul
//! tasso di astensione `astensioni / occasioni`: cresce con l'**esposizione** (più
//! volte i due layer sono stati co-toccati senza invertire la freccia, più ci
//! fidiamo) e crolla quando l'esposizione è minima. È statistica vera su un dato
//! vero, non un'euristica inventata.
//!
//! Ogni altro strumento misura cosa il codice *fa*. Noi misuriamo cosa il team ha
//! **scelto di non fare**: la preferenza rivelata, estratta dal negativo del tempo.
//!
//! # I due assi
//!
//! - **tempo** ([`abstention`]): *quanto* un invariante è battle-tested. Confidenza
//!   = lower bound di Wilson sul tasso di astensione.
//! - **intento** ([`fossil`]): *quando* e *perché* un confine è nato. Il
//!   [`DecisionFossil`] recupera dal commit più vecchio che ha co-toccato i due
//!   layer il diff strutturale di cristallizzazione e l'intento dichiarato (il
//!   messaggio dell'autore).
//!
//! # Struttura
//!
//! - [`history`]: la sorgente dei commit, astratta da un trait così l'analisi è
//!   testabile senza un repo git reale ([`GitLog`] per il vero git, [`InMemoryHistory`] per i test).
//! - [`abstention`]: la matematica pura — conteggio delle [`occasions`] e lower
//!   bound di Wilson su [`Abstention`].
//! - [`fossil`]: la datazione dei confini — [`excavate`] scava il [`DecisionFossil`].
//! - [`miner`]: l'estrazione del *perché esplicito* dai messaggi di commit —
//!   [`mine`] riemerge le decisioni che l'autore ha scritto a parole (verbatim,
//!   citando l'hash), astenendosi sui commit terse invece di inventare.
//! - [`adr`]: l'ingestione degli **ADR** (`docs/adr/*.md`) — [`mine_adrs`] legge le
//!   decisioni architetturali già deliberate, citando il file e astenendosi su
//!   template e ADR superati.

pub mod abstention;
pub mod adr;
pub mod fossil;
pub mod history;
pub mod miner;

pub use abstention::{
    boundary_story, occasion_window, occasions, Abstention, BoundaryOccasion, OccasionWindow, Z_95,
};
pub use adr::{is_adr_path, mine_adrs, read_adrs, AdrDoc};
pub use fossil::{excavate, is_history_insufficient, DecisionFossil};
pub use history::{head_commit, CachedHistory, Commit, CommitHistory, GitLog, InMemoryHistory};
pub use miner::{
    mine, read_commit_messages, CommitMessage, DecisionSource, IntentConfidence, MinedDecision,
};
