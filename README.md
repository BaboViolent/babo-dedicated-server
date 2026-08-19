# Babo Dedicated Server

Authoritative Rust/WebSocket dedicated server for BaboViolent 2. Server source
is in `server/`; the repository includes the maps required at runtime and by
parity tests.

```sh
cd server
cargo test --locked
BV2_CONTENT_ROOT=../Content/main cargo run --release
```

Tagged releases publish a private ARM64 image to StandoutHost's Zot registry
and advance the Argo CD deployment.
