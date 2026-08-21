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
wss://<server>/bot/ws
```

This endpoint uses the same framed binary BV2 protocol as `/ws`. External bots
must append a `BOT1` identity block to `NET_CLSV_GAMEVERSION_ACCEPTED`. After
the native 17-byte payload, write the ASCII magic `BOT1`, then four fields in
order: implementation name, version, author, and in-game player name. Each
field is one unsigned length byte followed by that many UTF-8 bytes. Maximum
lengths are 64, 32, 64, and 96 bytes respectively.

- the in-game name is independent of the implementation name;
- in-game names accept `^1` through `^9` colors, Unicode equivalents of Babo
  Alt-code glyphs, or native Babo control/glyph bytes;
- every in-game name is sanitized and receives exactly one ` [bot]` suffix;
- the complete display name remains within the native 31-byte wire limit;
- `/info` and CTF transition logs report the player as a bot;
- `/info` publishes the reported values as `scoreboard[].botIdentity`;
- the server does not drive its movement, aiming, weapons, team, or respawn;
- external bots occupy player capacity and replace a built-in server bot just
  like another independently controlled connection;
- bots cannot create or vote in server votes.

The connection flow is the normal client flow: receive `NET_SVCL_NEWPLAYER`
and `NET_SVCL_GAMEVERSION`, answer with the extended
`NET_CLSV_GAMEVERSION_ACCEPTED`, request a team, request a spawn, then send normal
coordinate and action packets. Servers may still require their configured
password in the version-accepted packet and apply the normal movement and
application rate limits.

Use `/ws` for humans and `/bot/ws` for bots. A bot cannot remove its marker by
renaming or by sending a human-style player-info packet.

Server-owned bots also publish an identity. Their implementation is reported
as `bv2-server AI`, with the running server package version and community
author, so every player marked as a bot can be attributed through `/info`.
