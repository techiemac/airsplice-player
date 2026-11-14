# media-play

A desktop video and music player written in Rust. The interface is built
with [GPUI](https://crates.io/crates/gpui), the GPU-rendered UI framework
behind the Zed editor, and uses Zed's One Dark look. Decoding uses Rust
libraries and the operating system's hardware decoders; there is no
FFmpeg, GStreamer or libVLC underneath.

**Status:** early (0.1). Developed and tested on macOS and Linux, including
the Raspberry Pi 5. The Windows build has not been tested yet.

## Features

- Playlist with tag titles, durations and total length; drop files or
  folders anywhere in the window
- Command palette and fuzzy file finder, keyboard-first controls
- Hardware media keys and the system Now Playing view (macOS Control
  Center, Windows media overlay, Linux MPRIS)
- Session restore: playlist, track, position, volume, repeat mode and panel
  layout come back (paused) on the next launch
- Hardware video decoding on macOS and Raspberry Pi
- Surround Opus; AAC encoder delay trimmed using MP4 edit lists
- Seeks land on the requested time, decoding forward from the previous
  keyframe

## Format support

**Containers:** MP4/MOV, Matroska/WebM, Ogg, WAV, AIFF, CAF, MP3, FLAC.

**Audio:** AAC, MP3, FLAC, Opus (up to 7.1), Vorbis, ALAC, PCM, ADPCM, on
every platform.

**Video:**

| Codec | macOS | Raspberry Pi 5 | Other Linux, Windows |
|---|---|---|---|
| H.264 | hardware | software | software |
| HEVC 8/10-bit | hardware | hardware | not supported |
| AV1 8/10-bit | hardware on M3 and later, otherwise software | software | software |
| VP9 | hardware | not supported | not supported |
| VP8 | not supported | not supported | not supported |

When a file's video codec is not supported, its audio still plays and the
player says why. The info panel (Cmd/Ctrl+R) shows which decoder is in use.

Software decoders: [rav1d](https://github.com/memorysafety/rav1d) for AV1
and Cisco's [OpenH264](https://github.com/cisco/openh264) for H.264
(compiled from bundled source; it is the one C++ component, since there is
no production-ready Rust H.264 decoder).

### Performance

Apple M5 Max, release build, decode and colour conversion, multiples of
real time:

| Stream | Hardware | Software |
|---|---|---|
| 4K30 H.264 | 5.9x | 1.1x |
| 4K30 HEVC 10-bit | 7.1x | |
| 1080p60 H.264 | 7.7x | 2.3x |
| 1080p30 AV1 | | 7.3x |

Raspberry Pi 5, frames shown on screen by the app:

| Stream | Result |
|---|---|
| 1080p60 HEVC | 60 fps |
| 4K30 HEVC 10-bit, window up to 1920x1080 | 30 fps |
| 4K30 HEVC 10-bit, full screen on a 4K display | about 18 fps |

## Building

Developed with Rust 1.92 (older versions are untested).

```sh
cargo run --release -- [files or folders...]
```

- **macOS:** Xcode command line tools. The separate Metal Toolchain is not
  needed (shaders compile at launch).
- **Linux:** a C++ compiler, libclang, and the development packages for
  ALSA, fontconfig, freetype, Wayland, X11/xcb, xkbcommon and Vulkan. On
  Debian and Ubuntu:

  ```sh
  sudo apt install build-essential libclang-dev libasound2-dev \
    libfontconfig-dev libfreetype-dev libwayland-dev libx11-xcb-dev \
    libxkbcommon-x11-dev libxcb1-dev libvulkan-dev
  ```

- **Windows:** Visual Studio Build Tools with the C++ workload.

### Raspberry Pi

The Pi 5 runs the player on Raspberry Pi OS (64-bit, desktop). Building on
the Pi itself works but takes close to an hour, so `scripts/pi.sh`
cross-builds in Docker on another machine and copies the result over SSH:

```sh
export PI_HOST=user@raspberrypi      # required
export PI_KEY=~/.ssh/id_ed25519      # optional: SSH key to use
export PI_DIR=/home/user/media-play  # optional: install directory on the Pi

scripts/pi.sh release                # build and copy the release binary
scripts/pi.sh run [files...]         # build, copy and launch it
scripts/pi.sh test [filter]          # run the test suite on the Pi
```

Hardware HEVC uses the Pi's stateless V4L2 decoder (`rpi-hevc-dec`) through
the kernel's request API. Only the Pi 5 has been tested; the Pi 4 has the
same decoder and may work.

## Usage

| Action | macOS | Linux / Windows |
|---|---|---|
| Play / pause | Space or K | Space or K |
| Seek back / forward 5 s | Left / Right, or J / L | Left / Right, or J / L |
| Volume | Up / Down, or scroll on the slider | Up / Down, or scroll on the slider |
| Mute | M | M |
| Previous / next track | P / N, Cmd+Left / Cmd+Right | P / N, Ctrl+Left / Ctrl+Right |
| Repeat off / all / one | R | R |
| Full screen | F, or double-click the video | F, or double-click the video |
| Open files | Cmd+O | Ctrl+O |
| Command palette | Cmd+Shift+P | Ctrl+Shift+P |
| Find in playlist | Cmd+P | Ctrl+P |
| Toggle playlist / info panel | Cmd+B / Cmd+R | Ctrl+B / Ctrl+R |
| Stop | Cmd+. | Ctrl+. |

Files passed on the command line replace the saved playlist. The session is
stored in `media-play/session.json` under the platform's config directory
(`~/Library/Application Support` on macOS, `~/.config` on Linux,
`%APPDATA%` on Windows).

Environment variables:

- `MEDIA_PLAY_SOFTWARE_VIDEO=1` disables hardware video decoding.
- `RUST_LOG=media_play=debug` logs why hardware decoding was skipped, decode
  errors, and the presented frame rate.

## Development

```sh
cargo test                  # tests on this machine
cargo clippy --all-targets  # lints
scripts/test-linux.sh       # clippy and tests on Linux, in Docker
scripts/pi.sh test          # tests on a Raspberry Pi (see above)
```

The tests decode small fixtures with known content through every backend
available on the machine, check timestamps and display order, run the full
playback engine against real audio devices (A/V sync, pause, seeking in
every container), and drive the UI through its real key bindings on GPUI's
test platform. Tests that need an audio device or a hardware decoder skip,
with a message, when there is none. Fixtures live in `tests/fixtures`;
`generate.sh` there recreates them.

`examples/` has tools for working on decoders: `bench_decode` and
`bench_playback` measure throughput and real-time delivery for any file,
`cmp_decoders` compares software and hardware output frame by frame, and
`dump_frames` writes raw decoded frames to a file for comparing decoders
across machines.

### Layout

| Path | Contents |
|---|---|
| `src/ui/` | GPUI views, theme, key bindings, session store, OS media controls |
| `src/media/player.rs` | playback engine: demux, decode and audio threads, clock, seeking |
| `src/media/audio.rs` | audio output, resampling, the playback clock |
| `src/media/video/` | video decoder trait, reorder buffer, colour conversion, backends |
| `src/media/video/hevc/` | Raspberry Pi V4L2 HEVC decoder |
| `src/media/mp4_timing.rs` | MP4 composition offsets and edit lists |
| `vendor/` | patched copies of upstream crates (each has a `PATCHED.md`) |

### Upstream issues worked around

These are worth fixing upstream; each workaround can go once they are.

- Symphonia 0.6.1's MP4 reader ignores composition offsets and edit lists,
  so its timestamps are decode times. `src/media/mp4_timing.rs` corrects
  them.
- Symphonia's MP4 reader rejects `avcC`/`hvcC` records over 1 KiB, which
  excludes most x265 output. Patched in `vendor/symphonia-format-isomp4`.
- Symphonia's MP4 and Matroska readers can seek past the target or into the
  middle of a GOP, and cannot seek after reaching the end. The engine
  verifies each seek lands on an earlier keyframe and reopens the file after
  end of stream.
- cros-codecs 0.0.6 panics on HEVC slices with more than 32 entry points
  (common in 4K). Patched in `vendor/cros-codecs`.
- xattr 0.2.3 (via GPUI) does not build against current libc on Linux.
  Patched in `vendor/xattr`.
- The published rav1d 1.1.0 crate is missing a header its ARM assembly
  needs, so it is built without assembly.
- OpenH264 deadlocks in threaded mode with the low-latency API, so software
  H.264 decodes on one thread.

## Known limitations

- No hardware video decoding on Windows (Media Foundation), on Linux PCs
  (VA-API), or for H.264 on the Raspberry Pi 4. HEVC and VP9 files play
  audio only on those systems.
- On the Pi 5, 4K HEVC full screen on a 4K display runs at about 18 fps.
- With PulseAudio's ALSA plugin, sound can start about 2 s late (video
  waits for it, so sync is kept). PipeWire does not show this.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The patched crates in `vendor/` keep their own licenses. Notably,
`vendor/symphonia-format-isomp4` is MPL-2.0, so changes to its files stay
under MPL-2.0.

The license covers this code, not codec patents. H.264 and HEVC are
patented, and because OpenH264 is built from source here, Cisco's patent
coverage for its own binaries does not apply.
