# rust:alpine is musl-hosted, so the default target is already
# *-unknown-linux-musl and links statically -- which is what lets the runtime
# stage be `scratch` rather than a distro base.
FROM rust:1.98-alpine AS build

RUN apk add --no-cache musl-dev

WORKDIR /src

# Dependencies first, against a stub main, so editing src/ does not rebuild
# ring and rustls every time.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# cargo skips a rebuild when only mtime changed; touching main.rs forces it to
# notice the real sources replaced the stub.
RUN touch src/main.rs && cargo build --release --locked

# No base image at all: rustls compiles the Mozilla root set in via webpki-roots,
# so there is no /etc/ssl to provide and nothing here but the binary.
FROM scratch

COPY --from=build /src/target/release/pve-events /pve-events

# nobody. Numeric so it resolves without /etc/passwd -- mount the state volume
# writable by this uid, or set PVE_EVENTS_STATE somewhere that is.
USER 65534:65534

ENV PVE_EVENTS_STATE=/data/state.json
VOLUME ["/data"]

ENTRYPOINT ["/pve-events"]
