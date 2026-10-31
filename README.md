<img width="1920" height="1080" alt="brag" src="https://github.com/user-attachments/assets/fa574bb6-7bd4-4e78-8b72-14c58e3c716e" />

<div align="center">

# Azeroth Survivors (LAN / ZeroTier / Tailscale Co-Op Edition)

**A free fan-made WoW roguelike by [Aethus](https://wow.aethus.pl/), run locally on a
from-scratch WoW 1.12.1 client engine (Rust + [Bevy](https://bevy.org)), extended with
4-player LAN / ZeroTier / Tailscale multiplayer.**

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg)](LICENSE-MIT)

</div>

> **Not an original project.** *Azeroth Survivors* is a free, fan-made WoW roguelike by
> **Aethus** — play the original in the browser at **[wow.aethus.pl](https://wow.aethus.pl/)**
> ([r/wowservers announcement](https://www.reddit.com/r/wowservers/comments/1x0yidl/azeroth_survivors_roguelike_wow_singleplayer_game/)
> · [Discord](https://discord.gg/UzxaZuSAk)). This repository is a from-source local build of the
> game, running on the [benilla](#how-its-built) / [wenilla](#how-its-built) engine, for personal use.

Pick a class and a battleground, then survive alone or in a **up to 4-player co-op party**: WASD
to move, your abilities cast themselves, enemies swarm in waves, you level up and pick upgrades,
and a boss shows up every couple of minutes.

<table>
  <tr>
    <td width="50%"><img src="docs/screenshots/menu.jpg" alt="The pick screen: race, gender and class on the left, sixteen zones on the right, the hero in between"></td>
    <td width="50%"><img src="docs/screenshots/combat.jpg" alt="A Mage holding off Elwynn Forest's wolves with frost, fire and arcane"></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/level-up.jpg" alt="Level up: three spell cards, coloured by rarity, each showing exactly what its next rank changes"></td>
    <td><img src="docs/screenshots/results.jpg" alt="The results screen after a death in Redridge Mountains"></td>
  </tr>
</table>

### In this build

- **LAN / ZeroTier / Tailscale 4-Player Co-Op:**
  - **Hybrid TCP + UDP Architecture:** Reliable TCP channel for lobby roster sync, synchronized
    team level-up card offers/locks, and phase transitions; low-latency 60 Hz UDP client inputs and
    30 Hz UDP world snapshots with client-side entity interpolation.
  - **Listen Server or Headless Dedicated Server:** Host directly from the in-game menu lobby or
    run a dedicated server process (`--server`) that automatically launches the zone once all
    connected party members mark Ready.
  - **Shared Team XP & Simultaneous Level-Up:** Any player's magnet pulls XP gems into the shared
    party XP bar. When the party levels up, the game pauses for everyone while each player selects
    from their own class-specific upgrade cards.
  - **Downed State & Proximity Revive:** Reaching 0 HP puts a hero into a downed state; living
    teammates can revive them by standing within 4 yards for 3 seconds. The run only ends if all
    party members are downed simultaneously.
  - **Dynamic Co-Op Scaling:** Enemy health scales by `+50%` and active swarm cap scales by `+35%`
    per additional player, with wave spawns distributed across living party members.
  - **Classic WoW Party HUD:** Left-side party frames, floating world-space teammate plates with
    cast/revive bars, and cyan/red minimap party indicators.
- **A character-create pick screen** built from the client's own glue art: eight races, both
  genders, every class the race can be, and **sixteen zones**, from the starting zones to
  Naxxramas and the Gates of Ahn'Qiraj.
- **Level-up cards** that deal in, tilt toward the cursor and lift on hover.
- **A class movement spell on F** from level 5, aimed at the cursor: Blink, Charge, Sprint, Dash,
  Fade, Ghost Wolf, Aspect of the Cheetah, Blessing of Freedom, Howl of Terror.
- **A camera you can free:** locked top-down by default with the hero facing the cursor. **C**
  unlocks it to swing and tilt, and the wheel zooms from over the shoulder out to 70 yards.

## Native Desktop & LAN Multiplayer (ZeroTier / Tailscale)

Build and launch the native desktop binary (`warcraft-survivors`):

```bash
# Run normally (use the top-center Co-Op Lobby panel in the menu to Host or Join):
cargo run --release -p warcraft-survivors

# Host a listen server directly on default port 25565 (TCP + UDP):
cargo run --release -p warcraft-survivors -- --host 0.0.0.0:25565 --name "Thrall"

# Join a friend's ZeroTier / Tailscale IP:
cargo run --release -p warcraft-survivors -- --join 100.64.12.34:25565 --name "Jaina"

# Run a headless/dedicated server on map index 0..15 (auto-starts when all joined players click Ready):
cargo run --release -p warcraft-survivors -- --server 0.0.0.0:25565 --map 0
```

Environment variable equivalents are also supported:
- `WOW_SURVIVORS_HOST=0.0.0.0:25565`
- `WOW_SURVIVORS_JOIN=100.64.12.34:25565`
- `WOW_SURVIVORS_SERVER=0.0.0.0:25565`
- `WOW_SURVIVORS_NAME=PlayerName`
- `WOW_SURVIVORS_MAP=0`

> **Firewall / VPN Note:** Make sure port `25565` (or your chosen port) allows both **TCP and UDP**
> traffic on your ZeroTier / Tailscale interface.

## Web Build (Single-Player)

```bash
git clone https://github.com/OutThisLife/azeroth-survivors-re
cd azeroth-survivors-re
make build     # fetch game data + install toolchain + build the wasm (~5.5 GB download)
make start     # → open http://127.0.0.1:8090/
```

## Controls

See [CONTROLS.md](CONTROLS.md). The short version: **WASD** to move, **Space** to jump, **F**
for your class's movement spell, the **mouse wheel** to zoom, **C** to unlock the camera, **Esc**
to pause, **1/2/3** to pick a level-up card.

## The game data (not in this repo)

This repo ships **no Blizzard content** — no art, models, sounds, maps, MPQ contents, or
FrameXML. The game reads a WoW 1.12.1 client's `Data` folder at runtime, which you supply.

`make build` (via `scripts/fetch-data.sh`) downloads the **English WoW 1.12.1 (build 5875)**
client from [archive.org](https://archive.org/details/WoW_1.12.1-5875) (~5.5 GB) and extracts the
MPQs into `wow-data/Data/` (git-ignored, resumable, skipped when already present). Or point at a
client you already have with `--data` / `WOW_DATA`.

## How it's built

The survivors mode is `crates/benilla-app/src/survivors/`, running on two upstream projects:

- [benilla](https://github.com/samwhosung/benilla) — a from-scratch WoW 1.12.1 client.
- [wenilla](https://github.com/Arnesen/wenilla) — benilla compiled to WebAssembly, with a local
  host and realm service.

## Legal

*Azeroth Survivors* is a free fan game by **Aethus**, not affiliated with or endorsed by
Blizzard Entertainment, and it ships no Blizzard game files. The screenshots in `docs/screenshots/`
show World of Warcraft art rendered from a user-supplied client; that art is © Blizzard
Entertainment. World of Warcraft is a trademark of Blizzard Entertainment, Inc. The engine code in
this repository is licensed under
[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option; the vendored components under
`third_party/` keep their own upstream licenses.
