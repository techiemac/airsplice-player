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
| Video, macOS: H.264, HEVC (8/10 bit), VP9, AV1 | VideoToolbox hardware decoding, via `objc2` bindings | OS |
| Video, software: AV1 (8 and 10 bit) | `rav1d` (port of dav1d) | Rust |
| Video, software: H.264 | `openh264` (Cisco, BSD, built from bundled source) | C++ |
| Audio output | `cpal` (CoreAudio, WASAPI, ALSA/PulseAudio/PipeWire) | Rust |
| Media keys, Now Playing | `souvlaki` (zbus on Linux) | Rust |
| Resampling | `rubato` | Rust |

The OS hardware decoder is used where there is one; otherwise H.264 and AV1
fall back to software (OpenH264 is the one non-Rust decoder: there is no
production-quality pure-Rust H.264 decoder yet). `MEDIA_PLAY_SOFTWARE_VIDEO=1`
forces software. The info panel shows which decoder is running.

**HEVC and VP9** need a hardware decoder: macOS has one (VP9 and AV1 on
recent Apple silicon). Windows (Media Foundation) and Linux (VA-API)
backends are not written yet; there, files with those tracks play their
audio and say why.

**Performance** (M5 Max, release build, decode plus BGRA conversion):

| Stream | VideoToolbox | Software |
|---|---|---|
| 4K30 H.264 | 5.9x real time | 1.1x (OpenH264) |
| 4K30 HEVC 10-bit | 7.1x | n/a |
| 1080p60 H.264 | 7.7x | 2.3x (OpenH264) |
| 1080p30 AV1 | hardware on M3+ | 7.3x (rav1d) |

OpenH264's threaded mode deadlocks with the low-latency API this player
uses, so software H.264 stays single-threaded.

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
cargo test               # this machine
scripts/test-linux.sh    # Linux, in Docker (clippy + full suite)
scripts/pi.sh test       # build on this machine, run the tests on a Raspberry Pi
```

`scripts/pi.sh` compiles in the same Docker image (64-bit ARM Linux) and
runs on the Pi, so the Pi's decoder hardware, PipeWire and display are
real. On an M5 Max an edit-compile-copy cycle takes about 25 s; the whole
suite runs on a Pi 5 in about 35 s. `scripts/pi.sh release` and
`scripts/pi.sh run [files]` build, copy and launch the release binary.
Set `PI_HOST` (`user@host`), `PI_KEY` (SSH key) and optionally `PI_DIR` to
choose the Pi; nothing about a particular machine is stored in the repo.

The Linux script builds in a Debian container with the CI job's packages
and plays audio into a PulseAudio null sink, so the audio-clock tests run in
real time.

- Unit tests: YUV conversion, avcC/Annex B, H.264 and AV1 keyframe parsing,
  OpusHead and surround channel order, resampling and downmix, fuzzy match.
- `tests/decode.rs`: every video codec on every available backend against
  fixtures with known content (1 s red, 1 s green, 1 s blue): frame count,
  timestamps, pixel colour; and B-frame streams must come out in display
  order.
- `tests/playback.rs`: the full engine: A/V sync on the audio and wall
  clocks, pause, seeks while paused, seeking back after the end in every
  container, unsupported-codec fallback. Tests that need an audio device
  skip (and say so) when there is none.
- UI tests drive the real view through the real keymap on GPUI's test
  platform.

Fixtures are in `tests/fixtures` (484 KB); `generate.sh` documents how they
were made.

`examples/bench_decode.rs` and `examples/bench_playback.rs` measure decode
throughput and real-time frame delivery for any file; `cmp_decoders.rs`
checks the software and hardware decoders produce the same picture at each
timestamp.

## Engine notes

- **Clock:** audio-driven when there is audio: frames consumed by the
  device, minus its buffer latency from cpal's callback timestamps (so
  deep device buffers do not desync video), with ~20 ms buffers requested.
  Otherwise a wall clock. Either clock holds after open and seek until the
  first frame is ready (pre-roll).
- **Seeking** uses a generation counter: the UI bumps it and resets the
  clock at once; every queued sample and frame is tagged and dropped if
  stale, so a seek never waits on a full queue.
- **Keyframes:** seeks land on a decodable frame regardless of container
  support, by backing off 1 s, 2 s, 4 s until the first video packet is a
  keyframe. Pre-roll frames are decoded but not colour-converted.
- **Display order:** decoders that emit decode order (OpenH264's
  low-latency API, VideoToolbox) feed a reorder buffer sized from the
  stream's SPS (`max_num_reorder_frames`, `sps_max_num_reorder_pics`), which
  is exact for conforming streams. As a last line of defence the frame
  picker never shows a frame older than the one on screen.
- **Workarounds for upstream issues** (worth reporting upstream):
  - Symphonia 0.6.1's MP4 reader ignores composition offsets (`ctts`) and
    edit lists (`elst`): `pts` is really the decode time, which put B-frame
    video in decode order and left AAC priming untrimmed.
    `src/media/mp4_timing.rs` reads those boxes and corrects timestamps.
  - Symphonia's MP4 reader rejects `avcC`/`hvcC` records over 1 KiB, which
    rules out most x265 files; `vendor/symphonia-format-isomp4` is a patched
    copy (see its `PATCHED.md`).
  - Symphonia's Matroska reader can seek to the cue point after the target,
    and its MP4 reader lands mid-GOP; seeks verify they landed on a keyframe
    at or before the target and back off otherwise.
  - Symphonia 0.6.1's MP4 and Matroska readers cannot seek after reaching
    end of stream; the engine reopens the file.
  - gpui 0.2.2 pulls in xattr 0.2.3, which does not build on Linux with
    current libc (the deprecated `ENOATTR` alias was removed);
    `vendor/xattr` is a patched copy (see its `PATCHED.md`).

## Known open items

- Through PulseAudio's ALSA plugin (as in the Docker test setup) the device
  pulls no audio for about 2 s after a stream opens; the clock holds, so
  sync is unaffected, but sound starts late. On a Raspberry Pi 5 desktop
  with PipeWire the playback tests show no such delay (inferred from their
  timing, not measured per stream).
- No hardware video backend on Windows (Media Foundation) or Linux: HEVC
  and VP9 play audio only there. On Raspberry Pi the hardware is a V4L2
  stateless HEVC decoder (`rpi-hevc-dec`) with column-tiled output; a
  backend is planned on cros-codecs' HEVC parser.
  - The published `rav1d` 1.1.0 crate omits `src/arm/asm-offsets.h`, so its
    assembly is disabled (pure Rust, still faster than real time at 1080p).
  - `opus-decoder` 0.1.1 was rejected: its multistream framing does not
    follow RFC 6716 Appendix B, and a `u8` shift in its CELT collapse mask
    overflows on real hybrid stereo frames.
