//! The caller's address, as a bucket key rather than as data.
//!
//! A rate limit needs "the same caller" to be one key, and it needs that key to be cheap. It does
//! not need the address itself: a header value, an identity and a bucket key are three different
//! things, and only the first two are the plugin's business. [`hash_address`] is therefore the only
//! way an address leaves [`HttpContext`](crate::HttpContext) — a `u64` that never names a person
//! and never leaves the process.
//!
//! The function is FNV-1a, not a cryptographic digest, and that is a deliberate property rather than
//! an omission: a generation must map one caller to one bucket (stability), the computation happens
//! per request (cost), and nothing outside this process ever sees the result (no threat model needs
//! preimage resistance). Swapping in a cryptographic hash would buy nothing here and cost a
//! dependency in a crate that must stay free of cryptography.

use std::net::IpAddr;

/// Offset basis of FNV-1a, 64 bit.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Prime of FNV-1a, 64 bit.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hashes one canonical address.
///
/// The caller canonicalises first, so IPv4 and its IPv4-mapped IPv6 spelling produce the same key: one
/// caller must not be able to hold two buckets by changing how it connects.
pub(crate) fn hash_address(address: IpAddr) -> u64 {
    match address {
        IpAddr::V4(address) => fnv1a(&address.octets()),
        IpAddr::V6(address) => fnv1a(&address.octets()),
    }
}

/// FNV-1a over a byte slice.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
