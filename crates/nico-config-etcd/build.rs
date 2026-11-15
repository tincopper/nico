//! Compiles the vendored etcd v3 protos into a gRPC server (the in-process mock etcd) and the
//! matching message types. Only the server side is used, by the test mock; the real client comes
//! from the `etcd-client` crate, which was itself generated from these same protos — so both ends
//! speak identical wire semantics.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/etcdserverpb/rpc.proto",
        "proto/etcdserverpb/kv.proto",
        "proto/etcdserverpb/auth.proto",
    ];
    let includes = ["proto/etcdserverpb"];
    tonic_build::configure()
        .build_client(false)
        .build_server(true)
        .compile_protos(&protos, &includes)?;
    Ok(())
}
