@echo off
setlocal EnableExtensions DisableDelayedExpansion
if not "%~1"=="" if /i not "%~1"=="pareto-residency" (
  echo Usage: Play-VIctory.cmd pareto-residency 1>&2
  exit /b 2
)
if not defined VICTORY_GAME_DIR set "VICTORY_GAME_DIR=%~dp0..\GTA6.Xbox.v1.0.2.1"
if not exist "%VICTORY_GAME_DIR%\game_scarlett_master.exe" (
  echo Set VICTORY_GAME_DIR to your own dump folder containing game_scarlett_master.exe. 1>&2
  exit /b 2
)
set "runner=%~dp0victory-3d-rtx-pareto-residency.exe"
if not exist "%runner%" set "runner=%~dp0target\release\victory.exe"
if not exist "%runner%" (
  echo Extract the Windows release or run cargo build --release --locked -p victory. 1>&2
  exit /b 2
)
if not defined VICTORY_AMBIENT_AIR set "VICTORY_AMBIENT_AIR=1.5"
if not defined VICTORY_CACHE_DIR set "VICTORY_CACHE_DIR=%LOCALAPPDATA%\VIctory\cache-pareto"
if not defined VICTORY_TEXTURE_NO_WAIT set "VICTORY_TEXTURE_NO_WAIT=1"
if not defined VICTORY_RELAY set "VICTORY_RELAY=2"
if not defined VICTORY_WIDE_BACK set "VICTORY_WIDE_BACK=1"
if not defined VICTORY_CARD_REHASH_RUNS set "VICTORY_CARD_REHASH_RUNS=1"
if not defined VICTORY_JOIN_COPIES set "VICTORY_JOIN_COPIES=1"
if not defined VICTORY_INDIRECT_WINDOW set "VICTORY_INDIRECT_WINDOW=1"
if not defined VICTORY_RELAY_COPY set "VICTORY_RELAY_COPY=1"
if not defined VICTORY_CARD_STEPS set "VICTORY_CARD_STEPS=2"
if not defined VICTORY_INDIRECT_BATCH set "VICTORY_INDIRECT_BATCH=2"
if not defined VICTORY_MAP_THREADS set "VICTORY_MAP_THREADS=1"
rem VICTORY_CARD_AWAKE=1 (a loop that keeps the card's clocks up) is not set here any more: it never
rem lets the card idle, so the system shows the card 100 % busy whatever the game draws. On an RTX
rem 5070 Ti the card was 45-65 % busy by the layer's own clock and never waited for (2026-11-26); what
rem the loop is worth there in frame time was not measured. Set VICTORY_CARD_AWAKE=1 before the
rem launcher to have it back, or VICTORY_CARD_AWAKE=turns to compare (20 s on, 20 s off, in the log).
if not defined VICTORY_SHOW_AFTER set "VICTORY_SHOW_AFTER=1"
rem Keep interiors and faces readable when the title's ray-traced bounce light is
rem unavailable.  One is the full scene-matched estimate; users can still set
rem VICTORY_AMBIENT themselves before launching to choose a different look.
if not defined VICTORY_AMBIENT set "VICTORY_AMBIENT=1"
if not defined VICTORY_THIN set "VICTORY_THIN=1"
if not defined VICTORY_JOIN_PARTS set "VICTORY_JOIN_PARTS=8"
if not defined VICTORY_UPLOAD_AHEAD set "VICTORY_UPLOAD_AHEAD=1"
if not defined VICTORY_LEVELS_ON_DEMAND set "VICTORY_LEVELS_ON_DEMAND=1"
if not defined VICTORY_CARD_BUFFERS_MIN_KB set "VICTORY_CARD_BUFFERS_MIN_KB=64"
rem The five passes that light the water from inside ("transmission volumetrics") do not translate for
rem a card with 32-lane waves (NVIDIA): they were dropped, and the water showed as a pale grey surface.
rem This gives them the translator's wave-wide form, which is off by default because it lost the card
rem once with two other shaders: 7 = all five (1 the two for a view from above the water, 2 two others,
rem 4 the one for a camera under water). 6, not 7: with the first two (bit 1, the tiled pair) the water
rem showed flickering black rectangles on an RTX 5070 Ti, gone with 6 (2026-11-26). If the game ends
rem with a graphics driver reset near water, set VICTORY_WAVE_WIDE_WATER=0 before the launcher.
if not defined VICTORY_WAVE_WIDE_WATER set "VICTORY_WAVE_WIDE_WATER=6"
rem Room on the card for textures: the layer fits it to the card it runs on (a tenth of the card's
rem budget kept for render targets, 768 to 3,072 MB, and textures loaded until about 1 GB of the
rem budget is left), and says what it picked in the log ("fitted to this card"). Fixed values tuned on
rem a 16 GB card filled a 12 GB one (2026-11-26). Set VICTORY_CARD_ROOM_FOR_TARGETS_MB or
rem VICTORY_TEXTURE_ROOM_PERCENT before the launcher to choose yourself.
rem A log of the run, outside the checkout (it holds what the title did): the info lines, among
rem them the frame line every few seconds that says where a frame's time went and how many draws
rem were dropped. VICTORY_PLAY_LOG=0: no file, and warnings on the console as before.
set "log="
if not "%VICTORY_PLAY_LOG%"=="0" (
  if not exist "%LOCALAPPDATA%\VIctory\logs" mkdir "%LOCALAPPDATA%\VIctory\logs"
  for /f %%t in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss"') do set "log=%LOCALAPPDATA%\VIctory\logs\play-%%t.log"
)
if not defined log goto console
if not defined VICTORY_LOG if not defined RUST_LOG set "VICTORY_LOG=info"
rem (A file takes the lines without the console's colour codes.)
if not defined NO_COLOR set "NO_COLOR=1"
echo The run's log: %log%
"%runner%" run "%VICTORY_GAME_DIR%\game_scarlett_master.exe" --console series-s --execute 2>"%log%"
exit /b %errorlevel%
:console
if not defined VICTORY_LOG if not defined RUST_LOG set "VICTORY_LOG=warn"
"%runner%" run "%VICTORY_GAME_DIR%\game_scarlett_master.exe" --console series-s --execute
exit /b %errorlevel%
