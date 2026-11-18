//! Identity codes (ADR-112): a number two owners read to each other to
//! check that the identity this computer holds for someone is the one
//! they hold for themselves, which PekoHub's word alone cannot show.
//!
//! A person's code comes from their account's device list (ADR-111): the
//! hash of its first link, which every later device extends and only a
//! reset replaces. A peko's code comes from its signing key (ADR-110).
//! Thirty digits in six groups of five, made the way Signal makes each
//! side of a safety number: SHA-512 iterated 5200 times over a version,
//! the kind, and the identity, then six 5-byte chunks, each modulo 100000.
//! A matching code for another identity takes about 2^100 hashes of work.

use sha2::{Digest, Sha512};

const VERSION: &[u8] = b"peko-code/1";
const ITERATIONS: usize = 5200;

/// Whose identity a code names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity<'a> {
    /// A person, by their device list's first link hash (base64url).
    Person(&'a str),
    /// A peko, by its signing key (`did:key:…`).
    Peko(&'a str),
}

impl Identity<'_> {
    fn kind(&self) -> &'static [u8] {
        match self {
            Self::Person(_) => b"person",
            Self::Peko(_) => b"peko",
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Person(s) | Self::Peko(s) => s.as_bytes(),
        }
    }
}

/// The code for `identity`: `12345 67890 12345 67890 12345 67890`.
#[must_use]
pub fn code_of(identity: Identity<'_>) -> String {
    let id = identity.bytes();
    let mut hash = {
        let mut h = Sha512::new();
        h.update((VERSION.len() as u32).to_be_bytes());
        h.update(VERSION);
        h.update((identity.kind().len() as u32).to_be_bytes());
        h.update(identity.kind());
        h.update(id);
        h.finalize()
    };
    for _ in 0..ITERATIONS {
        let mut h = Sha512::new();
        h.update(hash);
        h.update(id);
        hash = h.finalize();
    }
    hash[..30]
        .chunks(5)
        .map(|chunk| {
            let n = chunk.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
            format!("{:05}", n % 100_000)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_thirty_digits_and_depends_on_kind_and_identity() {
        let code = code_of(Identity::Peko("did:key:z6MkA"));
        assert_eq!(code.len(), 35);
        assert!(code
            .split(' ')
            .all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_digit())));
        assert_eq!(code, code_of(Identity::Peko("did:key:z6MkA")));
        assert_ne!(code, code_of(Identity::Peko("did:key:z6MkB")));
        assert_ne!(code, code_of(Identity::Person("did:key:z6MkA")));
    }

    /// The phone computes its account's code too: it checks this vector.
    #[test]
    fn matches_the_vector_the_phone_checks() {
        assert_eq!(
            code_of(Identity::Person("Zm9vYmFy")),
            include_str!("identity_code_vector.txt").trim()
        );
    }
}
