# syntax=docker/dockerfile:1

# Two stages, two roles, one artifact shape.
#
# The ROLE build argument is a *compile-time* decision, not a runtime switch: it decides whether
# `nico-llm` is linked in at all. A traffic image that could load a provider route would be a
# different product than the one this build produced, so nothing in the runtime image may change it.
#
#   docker build --build-arg ROLE=llm     -t nico:llm .
#   docker build --build-arg ROLE=traffic -t nico:traffic .
#
# See .trellis/tasks/11-13-deployment-shape/design.md for the invariants behind each stage.

FROM rust:1.99.0-slim-bookworm AS builder

# `ring` (the crypto provider behind the upstream TLS backend) builds C. No cmake, no ninja.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

ARG ROLE=llm
# Reject a typo at build time rather than silently producing the all-in-one binary. A role is an
# artifact property: a silent fallback would ship an image whose name lies about its contents.
RUN case "$ROLE" in \
      llm|traffic) ;; \
      *) echo "ROLE must be 'llm' or 'traffic', got '$ROLE'" >&2; exit 1 ;; \
    esac

WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# The deploy configs are copied in the runtime stage, not here; the build needs no configuration.

RUN if [ "$ROLE" = "traffic" ]; then \
      cargo build --release --locked -p nico-server --bin nico --no-default-features --features traffic; \
    else \
      cargo build --release --locked -p nico-server --bin nico; \
    fi

FROM debian:bookworm-slim AS runtime

# The runtime keeps the same libc line as the builder (`bookworm`), plus the platform roots that
# `rustls-native-certs` reads for upstream TLS. Without ca-certificates every real provider is
# unreachable while plaintext mock tests stay green — a failure that only appears in production.
RUN apt-get update \
    # ca-certificates for upstream TLS; curl so the compose healthcheck has a probe. Both are
    # runtime needs, not build leftovers: a slim image has neither a trust store nor an HTTP client.
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 nico \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin nico

COPY --from=builder /build/target/release/nico /usr/local/bin/nico
COPY --chmod=0444 deploy/bootstrap.yaml deploy/resources.yaml /etc/nico/

# The LKG cache is written by the process, so it lives outside the read-only config mount and
# belongs to the runtime user. A container rebuilt without this volume loses its last known good
# snapshot; compose mounts a named volume here.
RUN mkdir -p /var/lib/nico/cache && chown -R 10001:10001 /var/lib/nico

USER 10001:10001
WORKDIR /var/lib/nico

EXPOSE 8080
# metrics.listen is in the bootstrap file; 9090 is the sample's, not a second hardcoded listener.
EXPOSE 9090

# exec form: the binary is PID 1 and receives SIGTERM directly, which its own signal watcher turns
# into the drain. A `sh -c` wrapper would swallow that signal.
ENTRYPOINT ["nico"]
CMD ["--bootstrap", "/etc/nico/bootstrap.yaml"]
