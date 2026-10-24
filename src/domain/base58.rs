//! base58 (the Bitcoin alphabet, as Solana uses it) — hand-rolled, no crate.
//! Core: `secrets` uses it to tell a public on-chain id from a secret, the
//! Solana family (`domain::solana`, `lp`, the write tools) for every
//! `Pubkey` / `Signature`. Identifiers are always rendered in full.

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Encode bytes as base58. Leading zero bytes become leading `1`s.
pub(crate) fn bs58_encode(bytes: &[u8]) -> String {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    // Base-58 digits, little-endian.
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 138 / 100 + 1);
    for &byte in &bytes[zeros..] {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    out.extend(std::iter::repeat('1').take(zeros));
    out.extend(digits.iter().rev().map(|d| ALPHABET[*d as usize] as char));
    out
}

/// Decode base58. Errors on characters outside the alphabet.
pub(crate) fn bs58_decode(s: &str) -> Result<Vec<u8>, String> {
    let ones = s.bytes().take_while(|c| *c == b'1').count();
    // Base-256 bytes, little-endian.
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    for c in s.bytes().skip(ones) {
        let v = ALPHABET
            .iter()
            .position(|a| *a == c)
            .ok_or_else(|| format!("invalid base58 character {:?}", c as char))?
            as u32;
        let mut carry = v;
        for b in bytes.iter_mut() {
            carry += (*b as u32) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out = vec![0u8; ones];
    out.extend(bytes.iter().rev());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_edge_cases() {
        assert_eq!(bs58_encode(&[]), "");
        assert_eq!(bs58_encode(&[0, 0, 1]), "112");
        assert_eq!(bs58_decode("112").unwrap(), vec![0, 0, 1]);
        assert_eq!(bs58_encode(&[0xff; 4]), "7YXq9G");
        assert!(bs58_decode("0OIl").is_err());
    }
}
