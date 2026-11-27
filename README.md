# Vexport-Victory

Vexport-Victory is a fork of [VIctory](https://github.com/Marvisak/VIctory), a
compatibility layer that runs an Xbox Series title natively on Windows from files
the user supplies. This fork is tuned and fixed for an **NVIDIA RTX 50-series
card** (developed on an RTX 5070 Ti with 16 GB) and played with **keyboard and
mouse**.

**This repository contains no game files, code or assets.** You need your own
complete dump of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## Requirements

- Windows 10/11 x64, a CPU with AVX2
- A Direct3D 12 GPU with shader model 6.6 and mesh shaders (tested: RTX 5070 Ti 16 GB)
- 32 GB of RAM at least; close browsers and other large programs while playing
- The pinned Rust toolchain, the MSVC C++ build tools and a Windows SDK
- Your own dump of the title

## Build and run

```powershell
cargo build --release --locked -p victory
$env:VICTORY_GAME_DIR = 'D:\Games\MyDump'      # the folder with your copy's executable
.\Play-VIctory.cmd pareto-residency
```

`Play-VIctory.cmd` picks the Series S profile and the settings this fork was tested
with, and writes a log of each session to `%LOCALAPPDATA%\VIctory\logs`. The shader
cache lives in `%LOCALAPPDATA%\VIctory\cache-pareto`. The first launch with a new
build prepares the game's shaders for your PC before the game starts: a few minutes
with the processor fully busy, once. Without it the game does that work while you
play, at a few frames a second. `VICTORY_PREPARE=0` skips it, `VICTORY_PREPARE=1`
does it again. The first minutes in the world can still stutter while the graphics
driver builds what it needs; that goes away as its cache fills.

Copy `victory.example.toml` to `victory.toml` to keep your paths there instead; keep
every path outside this folder ([docs/asset-policy.md](docs/asset-policy.md)).

## Controls

The title is a console game and only knows a controller, so with no controller
plugged in the keyboard and mouse stand in for one, laid out like Grand Theft Auto
V's keyboard controls:

| Keys | On foot | In a vehicle |
|------|---------|--------------|
| W A S D | move | W throttle, S brake and reverse, A D steer |
| Mouse | look | look |
| Right / left mouse button | aim / fire | aim / fire |
| Left Shift | sprint | |
| Space | jump | handbrake |
| C, Left Ctrl | crouch | C looks behind |
| R | reload | |
| F | get in or out of a vehicle | get out |
| Q | cover | |
| E | context action | horn |
| Tab | weapon wheel | weapon wheel |
| V | camera view | camera view |
| Esc | pause | pause |
| Arrow keys | D-pad | D-pad |
| P (or F2) | switch between the two columns | |
| F1 | free the mouse pointer, and take it again | |

F switches to the driving keys as you get in and back as you get out. When the game
puts you in a vehicle itself (a mission start, after a cutscene), press P once. The
full table is in [docs/README-fxceless.md](docs/README-fxceless.md).

## Presets, picked for your PC

Nothing to set: at start the layer looks at the PC and says in the log what it
picked ("the processor's preset", "this PC, and the tier").

- **AMD processor**: the console's own kind of processor; the game's code runs
  as it is.
- **Intel processor**: the game uses a few AMD-only instructions, which the layer
  emulates. Its waits, which sleep the core on AMD, now give the core to another
  thread instead of spinning on it, so the CPU isn't held at full use while the
  game's threads wait. `VICTORY_CPU_PRESET=amd` or `=intel` picks one yourself.
- **Low tier**: for an 8 GB card or smaller, fewer than 8 CPU threads, or 16 GB
  of RAM or less. The game is told the display is 1280x720 instead of 4K, so it
  may draw fewer pixels (not tested yet: the log's "depth texture" lines show
  the size it renders at). `VICTORY_TIER=standard` or `=low` picks one yourself.
- **Frame rate (experiment)**: the game sets itself to 30 fps on a 60 Hz display.
  `VICTORY_DISPLAY_HZ=120` reports a 120 Hz display, which the game looks for
  before it allows faster frame intervals. Not tested yet: the log's "frame
  interval" line says what the game chose (33.3 ms is 30 fps, 16.7 ms is 60).
  With 120 Hz the game still chose 30 fps as a Series S. `VICTORY_CONSOLE=series-x`
  starts it as a Series X instead (more memory, more pixels): that one chose 30 fps
  too. `VICTORY_FPS=60` makes the layer's frame clock tick every 16.7 ms whatever
  the game asks for. Not tested yet: if the world moves too fast, the game counts
  frames instead of time, and the setting has to stay off.
- **Texture detail by card**: full on 16 GB cards; large textures at half detail
  on 12 GB cards; most textures at half detail on 8 GB cards.

## What this fork changes

- **Colour banding fixed**: the final colour grading reads its 3D tables with the
  blending done in the shader, which the RTX card's own filtering did not give
  (`VICTORY_VOLUME_FILTER=0` turns it off).
- **No grey wash**: the picture goes to the screen as the game encoded it, without a
  second tone curve (`VICTORY_DISPLAY_FILMIC=1` brings the old curve back).
- **Grey blocks in the sky fixed**: the game's clears of unordered access views are
  carried out (`VICTORY_CLEAR_UNORDERED=0`: not).
- **Skin painted right**: the textures a character's skin is painted from are loaded
  even when the card is full (`VICTORY_BAKE_OVER_ROOM=0`: not).
- **Fits itself to the PC**: the memory kept back for render targets, how full
  the card may get with textures, and how many threads translate and compile
  follow the card's memory and the processor count, so a 12 GB or 8 GB card
  and a 6- or 8-core processor are not run as hard as a 16 GB card and a
  16-thread one. On a card with less than 13 GB of budget (12 GB and smaller),
  textures of 1024 texels and more on a side are kept one level down, at half
  the detail and a quarter of the memory, because the game needs more than such a
  card has (`VICTORY_TEXTURE_DETAIL=full` keeps full detail, `=2048` drops only
  the largest). The log says what it picked ("fitted to this card", "texture
  detail", "pipeline workers").
- **Water lighting**: the water's transmission passes run on 32-lane cards
  (`VICTORY_WAVE_WIDE_WATER`; the launcher sets 6, see below).
- **Keyboard and mouse** laid out as above, with automatic driving keys.

## Known issues

- Two of the water's tiled lighting passes leave black rectangles on the water, so
  the launcher keeps them off: some stretches of water look white or foamy instead.
- Walls and floors can vanish for a single frame in some places.
- The card's memory fills up in busy scenes: some textures and models load late.
- Pale rectangles can show in the sky.
- The sun can show as a black disc.

## For developers

`CLAUDE.md` and [docs/architecture.md](docs/architecture.md) describe the code;
[docs/README-fxceless.md](docs/README-fxceless.md) lists the commands and debugging
switches. Every `VICTORY_...` switch is described in a comment where it is read.
`cargo test --workspace --locked` runs without game data, and `cargo xtask policy`
checks that nothing from the game is in the tree.

## Credits

This is a tree of other people's work with more on top.

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**:
  the AMD RX 7900 XTX fork this one is built on, with its speed work (list calls
  translated on threads of ours, joined submissions, the relay, card-side steps,
  frame record and replay; [docs/findings.md](docs/findings.md)).
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** and the
  people whose pull requests it carries.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray tracing fixes,
  cube gradients, sparse textures.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
  the ambient light idea, pipeline workers, and work by romainhedouin.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**.
- **[myokex/victory-amd](https://github.com/myokex/victory-amd)**: the guard over
  every view of a page (with Spexivus, joroyt and tightfall), and Joshua Odom's
  reads into title memory.
- **Geckomacabre** and **cxy0072019-sketch**: the texture read-back.
- **codec_d**: the Windows build and the RTX residency settings the launcher uses.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same
  title behaves under another layer; no code of it is here.
- Changes in this fork were made with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
