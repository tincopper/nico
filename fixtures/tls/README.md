# TLS fixtures

Workspace-shared test material, referenced from several crates (`nico-http`'s inbound-TLS tests,
`nico-config-etcd`'s real-wire tests, and `nico-server`'s bootstrap and end-to-end suites), which
is why it lives above any one of them. Include it as
`include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tls/<file>"))`, or resolve it
at runtime from the same path.

Two certificate authorities, two leaf certificates and the keys that go with them. **The private
keys here are test-only**: they exist so a loopback listener can complete a real handshake, and
nothing in this repository ever presents them to a peer that matters.

| File | What it is |
|---|---|
| `test-root.pem` / `.key` | The trust root the harness configures: `CA:TRUE`, self-signed. |
| `other-root.pem` / `.key` | A second, unrelated root. A process configured with this one does not trust the leaves below. |
| `localhost.pem` / `.key` | A leaf signed by `test-root`, `CN=localhost`, `subjectAltName = DNS:localhost, IP:127.0.0.1`. |
| `wrong-name.pem` / `.key` | A leaf signed by `test-root`, `CN=example.invalid`, `subjectAltName = DNS:example.invalid` only: a chain that verifies while the name does not. |

Every certificate is valid for 100 years from 2026-11-11, so no test depends on the wall clock it
runs at. They were generated once with `openssl` (3.6) — nothing regenerates them at test time,
which is what keeps a scenario free of certificate-synthesis dependencies:

```sh
gen_ca() { # <name> <common name>
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$1.key" -out "$1.pem" -days 36500 -subj "/CN=$2" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -addext "subjectKeyIdentifier=hash"
}
gen_leaf() { # <name> <common name> <subjectAltName> <issuing CA name>
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$1.key" -out "$1.csr" -subj "/CN=$2"
  printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=%s\n' "$3" > "$1.ext"
  openssl x509 -req -in "$1.csr" -CA "$4.pem" -CAkey "$4.key" -set_serial "0x$(openssl rand -hex 8)" \
    -out "$1.pem" -days 36500 -extfile "$1.ext"
  rm -f "$1.csr" "$1.ext"
}

gen_ca test-root "nico test root"
gen_ca other-root "nico other test root"
gen_leaf localhost localhost "DNS:localhost,IP:127.0.0.1" test-root
gen_leaf wrong-name example.invalid "DNS:example.invalid" test-root
```
