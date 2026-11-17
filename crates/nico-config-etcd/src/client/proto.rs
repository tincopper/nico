//! The generated etcd v3 server bindings, compiled from the vendored protos.
//!
//! Included under `cfg(test)` (this crate's own e2e suite) and under the `mock-server` feature
//! (integration tests in dependent crates): the mock server is a test harness, and keeping the
//! include feature-gated keeps tonic and prost out of the production link.

// Vendored-protocol output from tonic-build: the code is regenerated, never hand-edited, so every
// lint that exists for hand-written code is noise here.
#![allow(missing_docs, dead_code)]
#![allow(clippy::all, unreachable_pub)]

#[cfg(any(test, feature = "mock-server"))]
pub(crate) mod pb {
    // Two packages, one parent: `etcdserverpb` (the services) refers to `super::mvccpb` (the
    // key/value records its events carry), and both were generated from the same vendored protos.
    pub(crate) mod authpb {
        include!(concat!(env!("OUT_DIR"), "/authpb.rs"));
    }
    pub(crate) mod mvccpb {
        include!(concat!(env!("OUT_DIR"), "/mvccpb.rs"));
    }
    pub(crate) mod etcdserverpb {
        include!(concat!(env!("OUT_DIR"), "/etcdserverpb.rs"));
    }
}
