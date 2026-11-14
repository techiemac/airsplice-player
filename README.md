# media-play

A cross-platform media player with a Zed-style interface, built on
[GPUI](https://crates.io/crates/gpui) (Zed's own UI framework) and Rust
decoders. No ffmpeg.

## What's inside

| Layer | Crate | Language |
|---|---|---|
| UI | `gpui` (GPU-rendered: Metal, DirectX, Vulkan) | Rust |
| Demux: MP4/MOV, MKV/WebM, Ogg, WAV, AIFF, CAF, MP3, FLAC | `symphonia` | Rust |
| Audio: AAC, MP3, FLAC, Vorbis, ALAC, PCM, ADPCM | `symphonia` | Rust |
| Audio: Opus (incl. 5.1/7.1) | `ropus` (bit-exact port of libopus) | Rust |
| Video: AV1 (8 and 10 bit) | `rav1d` (port of dav1d) | Rust |
| Video: H.264 | `openh264` (Cisco, BSD, built from bundled source) | C++ |
| Audio output | `cpal` (CoreAudio, WASAPI, ALSA/PulseAudio/PipeWire) | Rust |
| Media keys, Now Playing | `souvlaki` (zbus on Linux) | Rust |
| Resampling | `rubato` | Rust |

H.264 is the one non-Rust decoder: there is no production-quality pure-Rust
H.264 decoder yet.

**Not supported:** HEVC and VP9 video (no cross-platform decoder without
ffmpeg). Files with those tracks play their audio and say so in the UI.

**Performance** (Apple Silicon, release build): every frame delivered for
720p, 1080p60 H.264 and 1080p AV1. 4K30 H.264 decodes at about 1.2x real
time, so slower machines will drop frames there. OpenH264's threaded
decoder deadlocks with the low-latency API this player uses, so it stays
single-threaded.

## Interface

Zed's One Dark theme and layout: title bar, playlist dock (left), the
video or now-playing view with a tab bar, info dock (right), status bar,
command palette and file finder. Drop files or folders anywhere.

- The playlist shows tag titles and durations (read in the background) and
  the total length; the file finder matches titles and file names.
- Hardware media keys and the system "Now Playing" view work on all three
  platforms (macOS Control Center, Windows media overlay, Linux MPRIS),
  via `souvlaki`.
- The session (playlist, track, position, volume, repeat, docks) is saved
  to `<config dir>/media-play/session.json` and restored paused on the next
  launch. Files given on the command line replace the saved playlist.

| Action | macOS | Linux / Windows |
|---|---|---|
| Play / pause | Space or K | Space or K |
| Seek -5 s / +5 s | Left / Right (J / L) | Left / Right (J / L) |
| Volume | Up / Down, scroll on slider | Up / Down, scroll on slider |
| Mute | M | M |
| Previous / next track | P / N, Cmd+Left / Cmd+Right | P / N, Ctrl+Left / Ctrl+Right |
| Repeat off / all / one | R | R |
| Full screen (video only view) | F, double-click video | F, double-click video |
| Open files | Cmd+O | Ctrl+O |
| Command palette | Cmd+Shift+P | Ctrl+Shift+P |
| Find in playlist | Cmd+P | Ctrl+P |
| Toggle playlist / info dock | Cmd+B / Cmd+R | Ctrl+B / Ctrl+R |
| Stop | Cmd+. | Ctrl+. |

## Build and run

```sh
cargo run --release -- [files or folders...]
```

- **macOS:** Xcode command line tools. GPUI's shaders compile at launch, so
  the separate Metal Toolchain download is not needed.
- **Linux:** `libasound2-dev libfontconfig-dev libfreetype-dev
  libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev libxcb1-dev
  libvulkan-dev` (Debian/Ubuntu names) and a C++ compiler.
- **Windows:** MSVC build tools (C++ workload).

## Tests

```sh
cargo test
```

- Unit tests: YUV conversion, avcC/Annex B, H.264 and AV1 keyframe parsing,
  OpusHead and surround channel order, resampling and downmix, fuzzy match.
- `tests/decode.rs`: every video codec against fixtures with known content
  (1 s red, 1 s green, 1 s blue): frame count, timestamps, pixel colour.
- `tests/playback.rs`: the full engine: A/V sync on the audio and wall
  clocks, pause, seeks while paused, seeking back after the end in every
  container, unsupported-codec fallback. Tests that need an audio device
  skip (and say so) when there is none.
- UI tests drive the real view through the real keymap on GPUI's test
  platform.

Fixtures are in `tests/fixtures` (484 KB); `generate.sh` documents how they
were made.

`examples/bench_decode.rs` and `examples/bench_playback.rs` measure decode
throughput and real-time frame delivery for any file.

## Engine notes

- **Clock:** audio-driven (frames consumed by the device) when there is
  audio, otherwise a wall clock that starts when the first frame is ready.
- **Seeking** uses a generation counter: the UI bumps it and resets the
  clock at once; every queued sample and frame is tagged and dropped if
  stale, so a seek never waits on a full queue.
- **Keyframes:** seeks land on a decodable frame regardless of container
  support, by backing off 1 s, 2 s, 4 s until the first video packet is a
  keyframe. Pre-roll frames are decoded but not colour-converted.
- **Workarounds for upstream issues** (worth reporting upstream):
  - Symphonia 0.6.1's MP4 and Matroska readers cannot seek after reaching
    end of stream; the engine reopens the file.
  - The published `rav1d` 1.1.0 crate omits `src/arm/asm-offsets.h`, so its
    assembly is disabled (pure Rust, still faster than real time at 1080p).
  - `opus-decoder` 0.1.1 was rejected: its multistream framing does not
    follow RFC 6716 Appendix B, and a `u8` shift in its CELT collapse mask
    overflows on real hybrid stereo frames.
