#!/usr/bin/env bash
# Build on this machine (64-bit ARM Linux in Docker), run on a Raspberry Pi.
#
#   scripts/pi.sh test [filter]   build the test binaries, run them on the Pi
#   scripts/pi.sh release         build the release binary, copy it to the Pi
#   scripts/pi.sh run [files...]  run the release build on the Pi's desktop
#
# PI_HOST (user@host; default pi@raspberrypi, Raspberry Pi OS's stock
# account) and PI_KEY (an SSH private key; default: ssh's own) select the Pi.
# PI_DIR (default /home/<user>/media-play) is where the project lives there.
# The container mounts the repository at that same path, so paths compiled
# into the tests (fixtures) are valid on the Pi. Binaries are built on
# Debian 12 and run on newer Pi OS releases.
set -euo pipefail
cd "$(dirname "$0")/.."

PI_HOST=${PI_HOST:-pi@raspberrypi}
if [[ "$PI_HOST" == *@* ]]; then pi_user=${PI_HOST%%@*}; else pi_user=$(whoami); fi
REMOTE_DIR=${PI_DIR:-/home/$pi_user/media-play}
SSH=(ssh -o BatchMode=yes)
[[ -n "${PI_KEY:-}" ]] && SSH+=(-i "$PI_KEY" -o IdentitiesOnly=yes)

build() {
  docker build -q -t media-play-linux -f scripts/linux.Dockerfile scripts >/dev/null
  docker run --rm \
    -v "$PWD":"$REMOTE_DIR" -w "$REMOTE_DIR" \
    -v media-play-pi-target:/target -e CARGO_TARGET_DIR=/target \
    -v media-play-linux-cargo:/usr/local/cargo/registry \
    -v "$PWD/target/pi":/out \
    -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_DEV_STRIP=debuginfo \
    media-play-linux bash -c "$1"
}

sync_sources() {
  # Fixtures and sources, so test paths resolve; never build output.
  rsync -az --delete --exclude target --exclude .git -e "${SSH[*]}" ./ "$PI_HOST:$REMOTE_DIR/"
}

on_pi() {
  # The desktop session's Wayland and PipeWire sockets, for UI and audio.
  "${SSH[@]}" "$PI_HOST" "export XDG_RUNTIME_DIR=/run/user/\$(id -u) WAYLAND_DISPLAY=wayland-0; cd $REMOTE_DIR && $1"
}

mkdir -p target/pi
case "${1:-}" in
  test)
    filter=${2:-}
    t0=$SECONDS
    # Test harness binaries only (not examples or the app); no debug info,
    # so each is a fraction of the size to copy.
    build "rm -rf /out/tests && mkdir -p /out/tests && cargo test --locked --no-run --message-format=json 2>/dev/null \
      | grep -E '\"profile\":\\{[^}]*\"test\":true' | grep -o '\"executable\":\"[^\"]*\"' | cut -d'\"' -f4 | xargs -I{} cp {} /out/tests/"
    t1=$SECONDS
    sync_sources
    "${SSH[@]}" "$PI_HOST" "mkdir -p $REMOTE_DIR/target/pi-tests"
    rsync -az --delete -e "${SSH[*]}" target/pi/tests/ "$PI_HOST:$REMOTE_DIR/target/pi-tests/"
    echo "built in $((t1 - t0))s, copied in $((SECONDS - t1))s"
    status=0
    for bin in target/pi/tests/*; do
      name=$(basename "$bin")
      echo "== $name"
      t=$SECONDS
      on_pi "target/pi-tests/$name $filter --test-threads=1 2>&1" || status=1
      echo "($name ran in $((SECONDS - t))s)"
    done
    exit $status
    ;;
  release)
    build "cargo build --locked --release && cp /target/release/media-play /out/media-play"
    "${SSH[@]}" "$PI_HOST" "mkdir -p $REMOTE_DIR/bin"
    rsync -az -e "${SSH[*]}" target/pi/media-play "$PI_HOST:$REMOTE_DIR/bin/media-play"
    echo "copied to $PI_HOST:$REMOTE_DIR/bin/media-play"
    ;;
  run)
    shift
    on_pi "bin/media-play $*"
    ;;
  *)
    sed -n '2,12p' "$0"
    exit 2
    ;;
esac
