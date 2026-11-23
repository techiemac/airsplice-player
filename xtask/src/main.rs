//! Build and test tasks that need more than cargo, run as `cargo xtask <task>`.
//!
//! Each runs in Docker, in the Debian 13 image from `linux.Dockerfile`
//! (glibc 2.41, as on current Raspberry Pi OS). On an ARM Mac the container
//! is native ARM64, so the Pi builds are not emulated. Build output is kept
//! in Docker volumes between runs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const USAGE: &str = "\
usage: cargo xtask <task>

  linux                   clippy and the test suite on Linux, with a
                          PulseAudio null sink as the audio device
  pi                      the Raspberry Pi app: target/pi/airsplice-player
  pi-tests [--dir PATH]   the test suite and its fixtures for the Pi:
                          target/pi/airsplice-player-tests.tar.gz. The tests
                          find fixtures by paths compiled into them, so the
                          bundle must be unpacked at PATH on the Pi
                          (default /home/pi/airsplice-player)

Nothing is sent to the Pi; copy the result over yourself, e.g. with scp.";

const IMAGE: &str = "airsplice-player-linux";
const REGISTRY: &str = "airsplice-player-linux-cargo:/usr/local/cargo/registry";

type Result<T = ()> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["linux"] => linux(),
        ["pi"] => pi(),
        ["pi-tests"] => pi_tests("/home/pi/airsplice-player"),
        ["pi-tests", "--dir", dir] => pi_tests(dir),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn linux() -> Result {
    let root = root();
    // Debug info off and link jobs limited: full debug links of GPUI exceed
    // Docker Desktop's default 8 GB VM and the linker gets OOM-killed.
    // RUSTFLAGS makes warnings errors in the vendored crates too, which
    // clippy's -D warnings does not reach.
    let c = Container::start(
        &[&format!("{}:/src", root.display()), "airsplice-player-linux-target:/target", REGISTRY],
        &[
            "CARGO_TARGET_DIR=/target",
            "CARGO_PROFILE_DEV_DEBUG=0",
            "CARGO_BUILD_JOBS=4",
            "RUSTFLAGS=-D warnings",
            // Where a desktop session's sockets are, so the app reaches
            // PulseAudio as it reaches PipeWire on a Pi.
            "XDG_RUNTIME_DIR=/run/user/0",
        ],
        "/src",
    )?;
    c.exec(&["mkdir", "-p", "-m", "700", "/run/user/0"])?;
    // A null sink: a real-time audio device without sound hardware, so the
    // audio-clock playback tests run.
    c.exec_quiet(&["pulseaudio", "--daemonize", "--exit-idle-time=-1", "--load=module-null-sink"])?;
    c.exec(&["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"])?;
    c.exec(&["cargo", "test", "--locked", "--", "--nocapture"])
}

/// A container for Pi builds, with the checkout mounted at `dir`: the path
/// compiled into the tests for their fixtures.
fn pi_container(dir: &str) -> Result<Container> {
    let root = root();
    fs::create_dir_all(root.join("target/pi")).map_err(|e| e.to_string())?;
    Container::start(
        &[
            &format!("{}:{dir}", root.display()),
            "airsplice-player-pi-target:/target",
            REGISTRY,
            &format!("{}:/out", root.join("target/pi").display()),
        ],
        &["CARGO_TARGET_DIR=/target", "CARGO_PROFILE_DEV_DEBUG=0", "CARGO_PROFILE_DEV_STRIP=debuginfo"],
        dir,
    )
}

fn pi() -> Result {
    let c = pi_container("/home/pi/airsplice-player")?;
    c.exec(&["cargo", "build", "--locked", "--release"])?;
    c.exec(&["cp", "/target/release/airsplice-player", "/out/airsplice-player"])?;
    println!("built target/pi/airsplice-player. Copy it over, e.g.:");
    println!("  scp target/pi/airsplice-player pi@raspberrypi:");
    Ok(())
}

fn pi_tests(dir: &str) -> Result {
    if !dir.starts_with('/') {
        return Err(format!("--dir must be an absolute path, not {dir}"));
    }
    let root = root();
    let c = pi_container(dir)?;
    // Test harness binaries only, not examples or the app.
    let out = c.output(&["cargo", "test", "--locked", "--no-run", "--message-format=json"])?;
    let mut tests = vec![];
    for line in out.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if msg["profile"]["test"] == true
            && let Some(exe) = msg["executable"].as_str()
        {
            tests.push(exe.to_string());
        }
    }
    if tests.is_empty() {
        return Err("cargo built no test binaries".into());
    }
    c.exec(&["rm", "-rf", "/out/tests"])?;
    c.exec(&["mkdir", "/out/tests"])?;
    let mut cp = vec!["cp"];
    cp.extend(tests.iter().map(String::as_str));
    cp.push("/out/tests/");
    c.exec(&cp)?;
    drop(c);

    let name = Path::new(dir).file_name().and_then(|n| n.to_str()).ok_or("--dir has no last component")?;
    let staging = root.join("target/pi/bundle");
    let bundle = staging.join(name);
    let _ = fs::remove_dir_all(&staging);
    copy_dir(&root.join("target/pi/tests"), &bundle.join("bin"))?;
    copy_dir(&root.join("tests/fixtures"), &bundle.join("tests/fixtures"))?;
    copy_dir(&root.join("crates/vp8/tests/vectors"), &bundle.join("crates/vp8/tests/vectors"))?;
    let runner = bundle.join("run-tests.sh");
    fs::write(&runner, RUN_TESTS).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let tarball = root.join("target/pi/airsplice-player-tests.tar.gz");
    run(Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(&staging).arg(name))?;

    let parent = Path::new(dir).parent().map_or("/".into(), |p| p.display().to_string());
    println!("built target/pi/airsplice-player-tests.tar.gz ({} test binaries). On the Pi:", tests.len());
    println!("  scp target/pi/airsplice-player-tests.tar.gz pi@raspberrypi:");
    println!("  tar -xzf airsplice-player-tests.tar.gz -C {parent} && {dir}/run-tests.sh");
    Ok(())
}

/// Runs on the Pi, which has no Rust toolchain, so it is plain shell.
const RUN_TESTS: &str = r#"#!/bin/sh
# Runs every test binary, one test at a time: ./run-tests.sh [filter]
# From an SSH session, uses the desktop's Wayland and PipeWire sockets.
cd "$(dirname "$0")"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
status=0
for bin in bin/*; do
  echo "== $(basename "$bin")"
  "$bin" "$@" --test-threads=1 || status=1
done
exit $status
"#;

/// A running container. Its main process reads a pipe from this one, so
/// it stops (and `--rm` removes it) however xtask exits, Ctrl-C included.
struct Container {
    name: String,
    _keepalive: std::process::Child,
}

impl Container {
    fn start(mounts: &[&str], env: &[&str], workdir: &str) -> Result<Self> {
        run(Command::new("docker")
            .args(["build", "-q", "-t", IMAGE, "-f"])
            .arg(root().join("xtask/linux.Dockerfile"))
            .arg(root().join("xtask"))
            .stdout(Stdio::null()))?;
        let name = format!("airsplice-xtask-{}", std::process::id());
        let mut cmd = Command::new("docker");
        cmd.args(["run", "-i", "--rm", "--name", &name, "-w", workdir]);
        for m in mounts {
            cmd.args(["-v", m]);
        }
        for e in env {
            cmd.args(["-e", e]);
        }
        cmd.args([IMAGE, "cat"]).stdin(Stdio::piped()).stdout(Stdio::null());
        let child = cmd.spawn().map_err(|e| format!("docker run: {e}"))?;
        let c = Self { name, _keepalive: child };
        // `docker run` returns before the container accepts commands.
        for _ in 0..100 {
            let up = Command::new("docker").args(["exec", &c.name, "true"]).stderr(Stdio::null()).status();
            if up.is_ok_and(|s| s.success()) {
                return Ok(c);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err("the container did not start within 10 s".into())
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.args(["exec", &self.name]).args(args);
        cmd
    }

    fn exec(&self, args: &[&str]) -> Result {
        run(&mut self.command(args))
    }

    fn exec_quiet(&self, args: &[&str]) -> Result {
        run(self.command(args).stdout(Stdio::null()).stderr(Stdio::null()))
    }

    /// Stdout as text; stderr (cargo's progress) passes through.
    fn output(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&capture(&mut self.command(args))?).into_owned())
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = self._keepalive.wait();
    }
}

fn run(cmd: &mut Command) -> Result {
    let status = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{cmd:?} failed ({status})")) }
}

fn capture(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.stderr(Stdio::inherit()).output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() { Ok(out.stdout) } else { Err(format!("{cmd:?} failed ({})", out.status)) }
}

fn copy_dir(from: &Path, to: &Path) -> Result {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|e| format!("{}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}
