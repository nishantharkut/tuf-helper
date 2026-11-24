# Contributing to TUF-Helper

Contributions focused on Linux laptop compatibility, DDC/CI monitor support, Slint UI clarity, and safe hardware handling are welcome.

---

## Development Setup

### Prerequisites (Ubuntu 24.04)

```sh
sudo apt update
sudo apt install -y build-essential pkg-config libfontconfig1-dev dbus appstream desktop-file-utils ddcutil
```

Ensure Rust (`cargo` and `rustc`) is available on `PATH` (install `rustfmt` and `clippy` as well if running advisory lint checks locally).

### Standard Checks & CI Gates

Strict blocking checks enforced by [`.github/workflows/ci.yml`](.github/workflows/ci.yml) on every pull request and push to `main`:

```sh
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
cargo doc --workspace --no-deps --locked
bash -n scripts/install.sh
desktop-file-validate packaging/tuf-helper.desktop
appstreamcli validate --no-net packaging/io.github.nishantharkut.TufHelper.metainfo.xml
```

Advisory lint and formatting checks (`continue-on-error: true` in CI):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Release binary verification (`cargo build --release --workspace --locked`) additionally runs on `main` pushes and tagged releases in [`.github/workflows/release.yml`](.github/workflows/release.yml).

Formatting follows [`rustfmt.toml`](rustfmt.toml) (`edition = "2021"`, `max_width = 100`) and [`.editorconfig`](.editorconfig).

---

## Guidelines

1. **Never trigger unsolicited hardware writes on startup:** Reading sensors, D-Bus properties, portal themes, and display snapshots must never mutate fan curves, profiles, power targets, refresh rates, or monitor VCP registers automatically (opt-in automation must remain disabled by default).
2. **Test against fixtures and private buses:** Hardware logic tests belong in unit/integration tests using synthetic sysfs trees (`crates/tuf-core/tests/safety.rs`), isolated private `dbus-daemon` instances (`crates/tuf-core/src/dbus/tests.rs`), or mock transports (`MockVcpTransport` in `display.rs`) — never write to live `/sys` or `/dev/i2c-*` inside `cargo test`.
3. **Preserve dGPU runtime power savings:** Telemetry and graphics detection must read sysfs (`/sys/class/drm`, `/sys/class/hwmon`) without spawning tools that force a suspended discrete GPU awake.
4. **Keep Slint layouts responsive at minimum width (`500px`) in Dark and Light themes:** Avoid hard-coded wide pixel columns or button overflows; verify narrow (`500px`) and default (`580px`) layouts in both Yaru Dark and Yaru Light without clipped labels or overlapping borders.
5. **Respect DDC/CI VCP timings, write dedup & error codes:** Monitor I2C transactions require proper settle delays (`50 ms` Get VCP, `60 ms` Set VCP, `45 ms` Capabilities), result-code (`reply[3] == 0x00`) checks, and read-before-write deduplication so unsupported features fail safely and unchanged registers do not wear monitor EEPROM.
