# bv2-server — BaboViolent 2 WebSocket dedicated server

Authoritative game server for the **jsclient** browser port. Speaks BaboNet
`netPacket.h` payloads over **WebSocket binary frames** (4-byte LE header +
payload). Default listen: **`:8080`**, path **`/ws`**.

Legacy C++ TCP/UDP BaboNet is not used.

## Run locally

```sh
# From repo root — Content/ must exist
export BV2_CONTENT_ROOT="$(pwd)/Content/main"

cd server
cargo run --release
```

### Deathmatch (default)

```sh
export BV2_MAP=DM-Candyland
export BV2_GAME_TYPE=0   # 0=DM  1=TDM  2=CTF
export BV2_BOTS=2
cargo run --release
```

### Capture The Flag

Use a map with flag pods (names usually start with `CTF-`):

```sh
export BV2_CONTENT_ROOT="$(pwd)/Content/main"
export BV2_MAP=CTF-BurialMound
export BV2_GAME_TYPE=2
export BV2_BOTS=4
export BV2_GAME_NAME="BV2 CTF"
RUST_LOG=bv2_server=info cargo run --release
```

Start the browser client and connect to `127.0.0.1:8080`:

```sh
cd jsclient && PORT=3000 node server.js
```

Open `http://localhost:3000` → Multiplayer → Connect → host `127.0.0.1`, port `8080`, pick a team, spawn.

| `BV2_GAME_TYPE` | Mode |
| --- | --- |
| `0` | Free-for-all deathmatch |
| `1` | Team deathmatch |
| `2` | Capture the flag |

Health: `curl http://localhost:8080/health`

Info JSON: `curl http://localhost:8080/info`. The response includes match and
round timers, team scores, the live scoreboard with measured ping and combat
statistics, and the 20 most recent public chat messages. Team chat is never
published by this endpoint.

WebSocket: `ws://localhost:8080/ws`

## Headless bot simulation

Run the authoritative bot loop without opening a network listener and emit a
single machine-readable JSON summary:

```sh
BV2_MAP=CTF-Alert BV2_GAME_TYPE=2 BV2_BOTS=12 \
  cargo run --release -- --bot-sim
```

`BV2_BOT_SIM_SEED` identifies the deterministic run,
`BV2_BOT_SIM_TICKS` controls its duration (default: 3000 ticks), and
`BV2_BOT_SIM_SCENARIO` selects `wall_occlusion`, `cover_retreat`, `separation`,
`carrier_escort`, `spawn_pressure`, or `full_ctf_round`. The JSON summary
reports tactical states, team roles, shots, accuracy, captures, returns,
deaths, rejected occluded shots, stuck recovery events, and a CI baseline
verdict.

## Environment

| Variable | Default | Description |
| --- | --- | --- |
| `BV2_BIND` | `0.0.0.0:8080` | HTTP + WebSocket listen address |
| `BV2_CONTENT_ROOT` | `../Content/main` | Maps and assets root |
| `BV2_MAP` | `DM-Candyland` | Current map name (no `.bvm`) |
| `BV2_GAME_TYPE` | `0` | Game mode id |
| `BV2_GAME_NAME` | `BV2 Web Server` | Server label |
| `BV2_PASSWORD` | *(empty)* | Join password |
| `BV2_MAX_PLAYERS` | `16` | Max clients (≤32) |
| `BV2_BOTS` | `2` | Debug bots spawned at startup (0 to disable) |
| `BV2_ENABLE_VOTE` | `false` | Allow votes for advertised maps and game modes |
| `BV2_WIN_LIMIT` | `7` | CTF captures required to win (`0` = unlimited) |
| `BV2_BOT_SIM_SEED` | `1` | Deterministic bot-simulation run identifier |
| `BV2_BOT_SIM_TICKS` | `3000` | Headless bot-simulation duration in ticks |
| `BV2_BOT_SIM_SCENARIO` | `full_ctf_round` | Named deterministic bot scenario |
| `RUST_LOG` | `bv2_server=debug` | Log level (`info`, `debug`, `trace`) |

## Docker

```sh
docker build -t bv2-server ./server
docker run --rm -p 8080:8080 \
  -e BV2_CONTENT_ROOT=/content/main \
  -v "$(pwd)/Content/main:/content/main:ro" \
  bv2-server
```

## Kubernetes / Istio

Expose port **8080** on the Service. Route **`/ws`** through Gateway +
VirtualService to this Service (long-lived WebSocket timeouts). Use
`/health` for liveness/readiness probes.

## Implemented (v0.2)

- WebSocket connect → handshake, spawn, team, coord relay
- **Authoritative hitscan** (map raycast + player sphere tests)
- **Server-owned fire timing and burst limits**, equipped/spawn weapon validation,
  and muzzle obstruction checks
- **Weapon parity** — two-trace sniper contract, deterministic legacy shotgun
  spread, and bots sharing human fire/respawn authority
- **Player hit / death / scoring** (DM + team FF rules)
- **Authoritative rocket, grenade, and molotov simulation** — server-owned IDs,
  movement, collision/bounce, fuses, explosions, flame burns, ammo, and cooldowns
- **Server-owned special weapons** — knife radius/line of sight, two-second
  half-damage shield, timed nuclear detonation, and flamethrower range/falloff
- **Authoritative movement validation** — legacy Y-then-X wall bounce and clip
  behavior, speed/displacement bounds, duplicate-frame rejection, corrected relay
- **Complete CTF match lifecycle** — configurable win limit, one-shot winner
  state, ten-second same-map reset, synchronized flags/scores, carrier cleanup
- Chat relay, ping/pong, timer sync, map list + chunk download
- **CTF flag state** (take/return/capture, bot objectives, net sync)

## Not yet ported

- Pickups and dropped-item projectile entities
- Admin and master listing
- Native client interop (TCP BaboNet)
