<div align="center">

# plavno

A lightweight TUI player for Linux and macOS with music streams from popular services, library shuffle, and mouse controls.

<img src="intro.png" alt="plavno terminal player with album artwork and an audio-reactive wave" width="885">

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

[Features](#features) · [Quick start](#quick-start) · [Services](#services) · [Controls](#controls)

</div>

## Features

- **MPRIS desktop integration (Linux)** — control playback with media keys, desktop media
  widgets, or `playerctl`. Track information and album artwork appear in desktop controls.
- **macOS media controls** — media keys and headphone buttons control playback;
  Now Playing publishes track information, artwork, playback position and speed.
- **Service waves and shuffle** — Yandex My Vibe, VK recommendations, library shuffle,
  and Wave Shuffle across connected services.
- **Likes and dislikes** — like/unlike in Yandex or VK, exclude unwanted tracks, and undo dislikes.
- **Mouse and hotkeys** — click controls, drag to seek, scroll for volume, and select track text to copy.
- **Listening history** — search past plays by title or artist and replay tracks.
- **RAM buffering and prefetch** — start playback while downloading; cache adjacent tracks for seeking and replay.
- **Audio-reactive visuals** — an animated wave with album artwork, toggled with `v`.

## Quick start

Install from crates.io with Rust. Linux also requires pkg-config, ALSA and D-Bus headers:

```sh
cargo install plavno --locked
plavno sources
```

Build dependencies: `libasound2-dev libdbus-1-dev` on Debian/Ubuntu, or
`alsa-lib-devel dbus-devel` on Fedora.

Alternatively, download a binary from [GitHub Releases](https://github.com/gmelikov/plavno/releases).
Linux x86_64 and ARM64 (`aarch64`) builds require glibc 2.35+, `libasound.so.2`, and `libdbus-1.so.3`
(`libasound2` / `libasound2t64` and `libdbus-1-3` on Debian/Ubuntu). Extract the archive
and run `./plavno`, or place it in `~/.local/bin`.

Audio uses the ALSA default device; PipeWire/PulseAudio needs its ALSA plugin.
Install ffmpeg for HLS playback. Optional Spotify and YouTube helpers are built separately.

On macOS, build from the checkout below after installing Rust and Xcode Command
Line Tools (`xcode-select --install`). Playback uses the CoreAudio default output;
ALSA and D-Bus are not required. Install ffmpeg with `brew install ffmpeg` for HLS.
Chrome/Chromium is detected in `/Applications`, `~/Applications`, or PATH.
Desktop media controls use MPRIS on Linux and MediaPlayer on macOS. The macOS
integration handles play/pause, stop, next/previous, position and playback speed;
it runs without a Dock icon or a separate window. Sixel rendering needs a terminal with sixel support;
the default braille renderer works without it.

To build from source:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
plavno sources
```

Connect a service below, then start it with `plavno --source SOURCE`.
For development, use `cargo run` or `cargo check` and keep the build cache.
CI checks the player and both helpers on Linux and macOS 15 (Apple Silicon and
Intel). `cargo test --locked --features test-audio` also runs the PTY UI tests on
macOS without a physical output device. Unit tests use a software audio sink;
the `test-audio` feature enables that sink in the test executable only when
`PLAVNO_TEST_AUDIO=1` is set. Normal builds use the platform's audio device.

## Services

| Service | Source key | Stable | Playback | Connection |
|---|---|:---:|---|---|
| Yandex Music | `yandex` | 🟢 | My Vibe, favorites shuffle, flow presets | OAuth token; full tracks require account access / Yandex Plus |
| VK Music | `vk` | 🟢 | Recommendations, My Music shuffle, flow presets | Installed Chrome/Chromium or manual session import |
| SoundCloud | `soundcloud` | — | Track stations | Browser sign-in or a public seed track |
| Spotify | `spotify` | — | Track radio | Optional Rust helper; Premium and OAuth |
| YouTube Music | `ytmusic` | — | Home mixes and track radio | Optional Rust helper; cookies or a public seed |
| Local music | `local` | — | Recursive directory shuffle | A local music directory |

### Yandex Music

Get an OAuth token from the [token guide](https://ym.marshal.dev/token), paste it
into `plavno login yandex`, then run `plavno --source yandex`.

### VK Music

Run `plavno login vk` and sign in through installed Chrome/Chromium, then run
`plavno --source vk`. Alternatively, [import a session manually](docs/services.md#manual-vk-session)
with `plavno login vk --manual`.

### SoundCloud

Run `plavno login soundcloud`, sign in through Chrome/Chromium, and choose a
seed track when prompted. Start with `plavno --source soundcloud`.
[Manual setup](docs/services.md#manual-soundcloud-session) also supports public track stations.

### Spotify

[Build the Spotify helper](docs/services.md#spotify-and-youtube-music), then run
`plavno login spotify` and paste a JSON profile with a seed track URI, OAuth token
with `streaming` scope, and client ID. Premium is required. Start with `plavno --source spotify`.

### YouTube Music

[Build the YouTube helper](docs/services.md#spotify-and-youtube-music), then run
`plavno login ytmusic` and paste `{"cookies":"/absolute/path/youtube-cookies.txt"}`
using a Netscape cookie export, or `{"seed":"VIDEO_ID_OR_URL"}` for public radio.
Start with `plavno --source ytmusic`.

See [service setup](docs/services.md) for profile examples and troubleshooting.

## Run

```sh
plavno sources                         # List supported services and connection status
plavno --source yandex                  # Yandex Music My Vibe
plavno --source yandex --favorites      # Shuffle Yandex favorites
plavno --source vk                      # VK recommendations
plavno --source vk --favorites          # Shuffle VK My Music
plavno --source mixed                   # Mix waves from connected services
plavno local ~/Music                    # Save the local music directory
plavno --source local                   # Shuffle the saved directory, including subdirectories
plavno --directory /path/to/music       # Use a different directory for this run
plavno --source mixed --directory ~/Music # Include local music in the mix
plavno --file /path/to/track.flac        # Play a local file
plavno --no-wave                        # Start without visualization
```

## Controls

Buttons display their hotkeys and support mouse clicks.

| Key | Action |
|---|---|
| Space / x | Pause or resume / stop |
| p / n | Previous / next track |
| ← / → | Seek 5 seconds; Shift or Ctrl: 30 seconds |
| Home / End | Start / end of track |
| + / - | Volume; also click beside the percentage |
| [ / ] | Playback speed: 0.1–2x, step 0.05; click buttons or scroll over Speed |
| l | Like / unlike in Yandex favorites or VK My Music |
| d / u | Dislike and skip / undo dislike |
| h or / | History: type to search, ↑/↓ select, Enter play |
| s / m | Choose a service / mix service waves |
| w / f | New wave / library shuffle (VK: My Music) |
| g | Flow: Calm, Balanced, Energetic, Work; choose with 1–4 or a click |
| v | Toggle visualization |
| b | Toggle a dancing silhouette over the wave; `--dance` starts with both |
| Ctrl+C | Copy selected text; quit when nothing is selected |
| Esc / q | Clear selection or close a view / quit |

Use `--dance` for a continuous, audio-reactive silhouette drawn in the same Braille
style as the wave, which stays visible. The prototype blends free-flowing gestures,
weight-shifting groove, and sharper pulse accents based on rhythm and energy.
Variable phrases mix club footwork, torso accents, stage travel, crouches, and jumps.
A 3D skeleton turns the body and head; projected limbs shorten naturally, with
depth-based occlusion in the CPU Braille renderer.
Track changes retain the pose and blend into the next movement character. Strong beats drive
body impulses; overcommitted moves can lead to a recovery step or a rare fall.
Subtle rim lighting and small face details help the figure stand out over album art.
`auto` mixes house-inspired footwork, hip-hop rocks, and rave-inspired steps using
tempo, energy, and available genre metadata (Yandex/SoundCloud). Ambient favors Flow.
Compare with `--dance-style house`, `hiphop`, or `rave`; `flow`, `groove`, and `pulse`
remain available as basic movement profiles.
`--renderer auto` and `--renderer braille` select terminal dots. To compare native
pixels in foot: `plavno --dance-style flow --renderer sixel`. The wave stays in
Braille; sixel draws the dancer over it. Run directly in the terminal if a
multiplexer hides its pixel dimensions.
Sixel uses a detailed violet-jacket skin; `--dance-skin silhouette` selects the
previous look. See [dance rendering](docs/dance.md) for the original pose sheet.
Sixel defaults to `--sixel-update diff`, restoring and redrawing only the changed
region. Use `--sixel-update full` to clear and repaint each frame instead.

Hover over the album artwork: the left third plays the previous track, the center
toggles playback, and the right third plays the next track. Click the progress bar
to jump, or drag it for audible 2–4x scrubbing. Scroll over the bar to seek, over
Speed to change the rate, and elsewhere to change volume. Drag over the title or
artist and press Ctrl+C to copy (requires terminal OSC 52 support). In History and
Sources, click to select, double-click to play, scroll to navigate, and right-click
to close.

Flow changes recommendations. Work requests calm ambient without vocals on Yandex
and calm instrumental tracks on VK. `w` returns from library shuffle to a new wave;
VK filters recent tracks to avoid repeating the initial queue.
Flow also works in Wave Shuffle: it changes supported sources, while other sources,
including local music, keep playing their full libraries without mood filtering.

Local music discovers MP3, FLAC, M4A/MP4, and Ogg Vorbis files recursively in the
background, visiting subdirectories in random order. Playback can start before
discovery finishes; the first tracks depend on which files have been discovered.
Tracks shuffle without repeats within a pass, then start a new pass without an
immediate boundary repeat when more than one playable track is available. Symlinks
are skipped. Files that cannot be opened or recognized are skipped when preparing
the queue. Titles and artists come from embedded tags, with filenames used when
the title is missing or blank. Embedded cover art is displayed in the TUI;
duration comes from the audio header. Tags are read only for queued tracks.
Press `w` to rescan and start a new shuffle. The file list stays in RAM; only the
configured directory is saved in `~/.config/plavno/local.json`.

## Playback and data

Playback starts while downloading, at the highest available supported quality.
Previous, current, and next audio files stay in RAM for seeking and replay;
RAM use depends on track size. Service audio is not saved to disk.

Configuration lives in `~/.config/plavno`; history and permanent dislikes live in
`~/.local/state/plavno`. `XDG_CONFIG_HOME` and `XDG_STATE_HOME` override these roots.
History stores metadata for the last 2000 plays. Yandex and VK also receive account
dislikes; `u` undoes them. Volume starts at 100%, speed at 1x. Changing speed also
changes pitch.

Media keys and `playerctl` work through session D-Bus, for example:
`playerctl -p plavno play-pause` or `playerctl -p plavno position 5+`.

## Checks

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Release steps: [Publishing to crates.io](docs/publishing.md).

## License

[Apache License 2.0](LICENSE).
