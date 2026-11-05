//! Any input at all, as a token.
//!
//! The property is not "a malformed token is refused" — that is one of the cases in `refusals`. It is
//! that nothing between the length check and the claim check indexes, slices or unwraps on an offset
//! taken from attacker-controlled input.

use proptest::prelude::*;

use super::*;

proptest! {
    /// Any string can be presented as a token, and no input may panic the verifier.
    ///
    /// The interesting part is not that a malformed token is refused — it is that nothing between
    /// the length check and the claim check indexes, slices or unwraps on attacker-controlled
    /// offsets.
    #[test]
    fn any_presented_bytes_are_refused_without_panicking(token in "\\PC*") {
        let filter = one_key();
        let _refused = filter.authenticate(&token, now());
    }

    /// The same, for input that is not even text: a header value the transport could not decode.
    #[test]
    fn arbitrary_bytes_are_refused_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let filter = one_key();
        let text = String::from_utf8_lossy(&bytes);
        let _refused = filter.authenticate(&text, now());
    }
}
