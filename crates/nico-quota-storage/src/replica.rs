//! The multi-replica contract: what "shared" must mean on every backend.
//!
//! These tests hold the deployment promise in one place: two independent holders of the same
//! backend see one window, an exhausted window refuses the next replica's admission, increments
//! from concurrent charges are never lost, a retried charge bills once, and expiry resets for
//! every replica. The fake drives them today; each adapter's suite drives the same scenarios
//! through its own wire, so a backend that disagrees with the promise fails here first.

#[cfg(test)]
mod tests;
