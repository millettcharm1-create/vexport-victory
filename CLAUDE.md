# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

VIctory is a compatibility layer that runs an Xbox Series title natively on Windows from a dump the user supplies. The docs and code say "the title"; it is the Xbox Series build of Grand Theft Auto VI. The title's x86-64 code runs unchanged inside the `victory` process. This workspace supplies what lies underneath it: a loader, the console's system APIs, D3D12.X on host D3D12, and a recompiler from the console GPU's machine code to DXIL.

It is a fork of several forks (see "Credits" in `README.md`), GPL-3.0-or-later, and Windows-only in practice (`x86_64-pc-windows-msvc`; most of `victory-gpu` is `#[cfg(windows)]`).

Read these before working on an area: `README.md` (state, how to run), `docs/architecture.md`, `docs/findings.md` (where a frame's time goes), `docs/README-fxceless.md` (commands and debugging switches), `docs/replay.md`, `docs/fork-ray-tracing.md`, `docs/bug-journal.md` (the newest work).

## Rule #1: nothing from the game enters the repository

`docs/asset-policy.md` is the full rule. In practice:

- Never write game-derived output inside the source tree: converted assets, prepared images, shader caches or dumps, captures, screenshots, logs that hold game data. The tools call `victory_core::ensure_outside_repo` and refuse; the tree is recognised by the `.victory-root` marker file.
- Never put such material into source, tests, fixtures or docs: disassembly, shader dumps, extracted tables, strings. Allowed: our own code for formats, build hashes, per-build offsets and byte-pattern signatures, small interoperability constants, synthetic test data.
- `cargo xtask policy` is an allowlist over file type, size (640 KiB) and content (no binary data, no hex or base64 run over 512 characters). It also runs as the test `repository_passes_policy` in `cargo test --workspace`, and in the pre-commit hook once `cargo xtask install-hooks` was run. A new file type or a data file needs an entry with a reason in `xtask/src/policy.rs`.
- Tests that need real files take their paths from environment variables and skip when those are unset. They assert on properties, never on snapshots.
- The user's paths are in `victory.toml` (git-ignored; copy `victory.example.toml`), read from the current directory, or in `VICTORY_GAME_DIR`, `VICTORY_CACHE_DIR`, `VICTORY_DATA_DIR` and `VICTORY_TEST_TITLES`. The defaults are `%LOCALAPPDATA%\victory\cache` and `%LOCALAPPDATA%\victory\data`.

## Commands

```powershell
# Builds
cargo build --release --locked -p victory                               # target\release\victory.exe, no debugging aids
cargo build --locked --profile diag -p victory --features diagnostics   # target\diag\victory.exe, the development build

# What CI runs (.github/workflows/ci.yml)
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked                                         # needs no game data

# One crate, one test (test names are sentences; a part of the name is enough)
cargo test -p victory-shader --lib --locked a_guarded_access -- --test-threads=1
cargo test -p victory-gpu --features diagnostics                        # with the frame recorder and replayer tests

cargo xtask policy                                                      # the asset-policy check alone
cargo run -p victory -- config                                          # the resolved paths, checked against the policy
```

About the tests:

- Many tests of `victory-gpu` and `victory-shader` make a D3D12 device and run on the PC's GPU, on WARP, or on both; each skips when there is no device. `VICTORY_WARP=1` puts the shader tests' host on WARP.
- Shader tests validate their DXIL with `dxil.dll` when it is found. An old Windows SDK's validator rejects `ps_6_6`: point `VICTORY_DXIL` at a current `dxil.dll`.
- The differential shader tests need the console SDK's compiler (`VICTORY_CONSOLE_DXC`) and skip without it.
- Benchmarks and the big tests are `#[ignore]`d and say in their header how to run them, for example `cargo test -p victory-gpu --release --lib indirect::bench -- --ignored --nocapture`.

## Running the title

```powershell
target\release\victory.exe run <game dir>\<the title's exe> --execute [--console series-s]
scripts\play.ps1 -Exe <the title's exe> [-Safe] [-RayTracing] [-Build release]
.\Play-VIctory.cmd pareto-residency
```

- `run` without `--execute` prepares, maps and binds, prints what is missing, and stops: nothing of the title runs.
- With `--execute` the title takes the GPU and tens of GB of memory for as long as it runs. A GPU fault can reset the display driver, and `victory replay` can lose the device too. Do not start the title, `victory replay` or `victory warm` unasked: ask the user first.
- `scripts\play.ps1` holds the settings the RX 7900 XTX measurements were made with (the diag build unless `-Build release`) and clears the shell's other `VICTORY_*` variables first. `Play-VIctory.cmd pareto-residency` holds the Series S profile with the memory-residency settings tested on RTX cards; it needs `VICTORY_GAME_DIR` and sets `VICTORY_LOG=warn`.
- Ahead of a first run, `victory shaders <the shader archives>` translates every shader into the cache (minutes), and `victory warm --used` makes the host pipelines that earlier runs drew with (after a new driver or a changed translator).
- What a run leaves: the log on stderr (`VICTORY_LOG=victory_gpu=debug,info`; debug and trace exist only in diagnostics builds); a frame line from `victory_gpu::host` every few seconds with the frame time, where it went and how many draws were dropped; and `crashes.log` in `<data dir>\<title id>\`, to which every build appends crashes and host GPU failures.

## Architecture

`docs/architecture.md` has the diagram and the table of crates. What takes several files to see:

### Getting the title to run (`victory-loader`, `victory-hle`)

- `prepare` patches a copy of the exe (marked as a DLL, no entry point, no imports, TLS callbacks detached), so that the Windows loader maps and relocates it and runs none of it. The copy goes to `<cache>\images`.
- Every import is then bound by us, in this order: an implementation in `victory-hle::modules` (always wins); the host's own function if `HostPolicy` passes that DLL through; an export of a DLL the title ships; a generated stub that names the import when it is called (`--unimplemented trap|zero`).
- To add a console API: write an `extern "system"` function named exactly like the import and list it with `hle_export!` in its module's `module()`; `modules/mod.rs` registers the modules. `victory census --missing` lists what titles import that nothing covers.
- The runtime answers as an offline retail console with one signed-in user. `modules/net.rs` lets the title reach loopback only, and DLLs that would open another way out (`rpcrt4`, Media Foundation's source resolver) are implemented or proxied here instead of passed through. Keep it that way when adding to `HostPolicy`.
- Title memory comes from `modules/xmem.rs`: host memory at the addresses the console would give (40 bits by default), reserved with write watching, with the title's physical pages as sections that it maps at several addresses.

### D3D12.X on host D3D12 (`victory-gpu`)

- The device and every object are laid out as the title's inlined D3D12.X code expects (`object.rs`). Lists keep their packet limits at zero, so every command arrives as a method call. A method not written yet logs its table offset and fails: a run's log says what to implement next.
- Console memory is unified: a GPU address is a CPU address, and each side reads what the other wrote. The host GPU cannot read title memory, so everything one side writes and the other reads has to be noticed and moved. Most of the crate is that:
  - a title buffer a draw uses gets a host copy (`shadow.rs`); a texture gets a host texture, detiled from the console's layout (`texture.rs`, `victory-formats`);
  - the title's writes are found by write watching or, in views of physical pages, by read-only guards on 64 KiB blocks and a vectored exception handler (`watch.rs`, `titlewrite.rs`);
  - before a submission runs, the ranges it reads are refreshed from title memory;
  - what shaders wrote is found by compare and hash passes over the ranges the submission declared as written, and put back into title memory as the dwords that changed (`land.rs`, `safecopy.rs`), before the title is told that the submission is done;
  - the title's descriptor heaps are mirrored slot for slot (`descriptor/host.rs`), because translated shaders index the host heaps at the title's own positions.
- A frame's path:
  1. The title makes tens of thousands of list calls. With `VICTORY_THIN` they are only written down, and worker threads translate them (`list/thin.rs`); `list.rs` and `draw.rs` record onto a host list.
  2. The title submits. With `VICTORY_ASYNC_SUBMIT` the submission joins one ordered line with the fence signals and the presents, served by a thread of ours (`queue/behind.rs`).
  3. `Host::submit` in `host.rs` runs the lists' submission-time steps (`HostList::run_deferred`: refreshes, copies, fills, uploads); some are done on the card instead (`host/card_steps.rs`, `host/place.rs`).
  4. With `VICTORY_LATE_WAIT` or `VICTORY_RELAY` the submission is sent ahead of the wait, and a relay thread lands the results in order (`Host::relay_land`).
  5. The present, and frame pacing (`frame.rs`).
- Host pipelines are made on a pipeline's first draw, by worker threads within a wait budget per frame (`pipeline/make.rs`). A draw or dispatch whose pipeline is not ready or did not translate is dropped. Pipelines are kept in a library on disk (`pipeline/library.rs`); `VICTORY_PIPELINE_BOOK=1` writes down every pipeline the title creates so that `victory warm` can make them ahead, in child processes (`pipeline/known.rs`).
- The title traces rays with its own compute programs over its own structures, through no ray-tracing API. `docs/fork-ray-tracing.md` covers the translated builder (`VICTORY_BVH_BUILDER=1`), title memory by address (`memory.rs`) and the stand-in for indirect light (`ambient.rs`).

### Shaders (`victory-shader`)

`emit::translate` takes the container bytes the title passes at pipeline creation:

1. `native.rs`: the GFX10 program the console's compiler stored beside the DXIL bitcode.
2. `ir.rs`: SSA, one instruction per machine instruction (`spill.rs` folds in a spilled user-data table).
3. `structure.rs`: the program as one lane runs it; EXEC lane masks become per-lane booleans and guards.
4. `resource.rs`: every descriptor and memory address traced back to a root parameter.
5. `io.rs`: what the inputs, the interpolation and the exports mean.
6. `emit/`: DXIL of shader model 6.6 through `lower.rs`, clean-up passes in `bitcode/opt.rs`, the container in `container.rs`.

- The header of `emit/mod.rs` is the binding contract between translated shaders and `victory-gpu` (`HostShader`: user data at `b0`, directly indexed heaps, root views, static samplers). Change both sides together.
- Console compute programs may be compiled for waves of 64 lanes. A host whose waves are 32 (NVIDIA) gets other forms of them, tried in turn by `emit` (`structure_wide`, `emit/own_bits.rs`, `emit/wave.rs`). The device hands the host's facts to the translator when it is made (`set_host_max_wave`, `set_host_rdna`, `set_view_heap_limit`).
- A translation is cached on disk under a key made of the container's bytes and a fingerprint of the translator: every file in `crates/victory-shader/src`, its `Cargo.toml`, `shader_cache.rs`, `Cargo.lock` and `rust-toolchain.toml` (see both `build.rs` files), the value of every `VICTORY_*` switch the translator reads, the build kind and the host's facts. So any edit to the translator makes every cached translation stale: `victory shaders` takes its minutes again, and host pipelines compile again (`victory warm`). `build.rs` takes every string literal `"VICTORY_..."` in `victory-shader/src` for a switch that joins the key.
- `victory translate [paths] --list --validate --pipelines` runs every shader found through the translator offline and tallies what it cannot handle. The unit tests run synthetic GFX10 programs on the host GPU and on WARP.

### Diagnostics builds

The `diagnostics` feature gates the debugging aids: the D3D12 debug layer and device-removed data, frame capture, the timeline, the sampling profiler, switches flipped while the title runs (`VICTORY_AB`), and the frame recorder with `victory replay` (`docs/replay.md`). A release build has none of them.

## Conventions

- **Header notes.** Every file opens with a `//!` note that gives the design, the reasons and what was measured, often naming a numbered run ("run light-10") whose data is not in the repository. Read it before changing the file, and keep it true.
- **Switches.** Behaviour is chosen by several hundred `VICTORY_*` environment variables, each read once and described in the comment where it is read. There is no list of them: search for the name. `switch::on(name)` is on when the variable is set and not `0`; `toggled!("NAME")` is on unless it is `0`. Most newer changes sit behind a switch that is off by default, and the two launchers hold the combinations that were tested.
- **Words.** "The title" is the game. "The card" is the host GPU, and "the PC's memory" is system memory. A "host copy" mirrors a title buffer, and a "refresh" brings one up to date. A "flight" is a submission sent ahead that the card has not finished ("in the air"); "landing" puts what its shaders wrote into title memory; to "settle" is to wait for every flight and land it. A "piece" is 64 KiB of a copy. "Declared" ranges are those a submission's shaders may write. A "picked" view is one a shader chooses from the heap by an index it computes. A "guard" is the read-only protection on a block of a view.
- **`shortcut:`** in a comment marks where the layer approximates the console, and what that leaves out.
- **"decision 0001 / 0003 / 0007"** in comments are design records that are not in this tree.
- **Figures are of one PC.** The README's numbers and most comments' are from an RX 7900 XTX; those of `docs/bug-journal.md` are from an RTX card. Some code is one vendor's alone (`book.rs` decodes AMD's descriptors; wave sizes differ by vendor).
- **Formatting.** Do not run `cargo fmt` over the workspace: `rustfmt.toml` asks for 100 columns, but about 120 files are not formatted to it and CI does not check formatting. Format what you write by hand, in the style of the file.
- **Lints.** `unsafe_op_in_unsafe_fn` and `unused_must_use` are denied, and CI fails on any clippy warning, with and without `--all-features`. Unsafe blocks carry a `// SAFETY:` comment (the lint for it is off in this fork; the practice stands).
- **Provenance.** Code taken from another fork or project names its source, commit and licence in the file's header (`queue/behind.rs`, `cores.rs`, `console_insn.rs`). Keep that when moving such code, and add it when bringing more in.
