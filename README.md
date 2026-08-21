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

## External bots

Programs can join as independently controlled bots at:

```text
wss://<server>/bot/ws?name=MyBot&version=1.0.0&author=YourName
```

This endpoint uses the same framed binary BV2 protocol as `/ws`. The URL-encoded
`name` (1-64 bytes), `version` (1-32 bytes), and `author` (1-64 bytes) identify
the bot implementation and are required before the WebSocket upgrade. The
server marks the connection as an external bot and owns its visible identity:

- every submitted name is sanitized and receives exactly one ` [bot]` suffix;
- the complete display name remains within the native 31-byte wire limit;
- `/info` and CTF transition logs report the player as a bot;
- `/info` publishes the reported values as `scoreboard[].botIdentity`;
- the server does not drive its movement, aiming, weapons, team, or respawn;
- external bots occupy player capacity and replace a built-in server bot just
  like another independently controlled connection;
- bots cannot create or vote in server votes.

The connection flow is the normal client flow: receive `NET_SVCL_NEWPLAYER`
and `NET_SVCL_GAMEVERSION`, answer with `NET_CLSV_GAMEVERSION_ACCEPTED`, send
`NET_CLSV_SVCL_PLAYER_INFO`, request a team, request a spawn, then send normal
coordinate and action packets. Servers may still require their configured
password in the version-accepted packet and apply the normal movement and
application rate limits.

Use `/ws` for humans and `/bot/ws` for bots. A bot cannot remove its marker by
renaming or by sending a human-style player-info packet.

Server-owned bots also publish an identity. Their implementation is reported
as `bv2-server AI`, with the running server package version and community
author, so every player marked as a bot can be attributed through `/info`.
