# Contributing to TUF-Helper

Contributions focused on Linux laptop compatibility, DDC/CI monitor support, Slint UI clarity, and safe hardware handling are welcome.

---

## Development Setup

### Prerequisites (Ubuntu 24.04)

```sh
sudo apt update
sudo apt install -y build-essential pkg-config libfontconfig1-dev dbus ddcutil
```

Ensure Rust (`cargo` and `rustc`) is available on `PATH`.

### Standard Checks

Run every check before opening a pull request:

```sh
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
cargo doc --workspace --no-deps --locked
cargo build --release --workspace --locked
bash -n scripts/install.sh
```

Formatting follows [`rustfmt.toml`](rustfmt.toml) (`edition = "2021"`, `max_width = 100`) and [`.editorconfig`](.editorconfig).

---

## Guidelines

1. **Never trigger unsolicited hardware writes on startup:** Reading sensors, D-Bus properties, and display snapshots must never mutate fan curves, profiles, power targets, or monitor VCP registers automatically.
2. **Test against fixtures and private buses:** Hardware logic tests belong in unit/integration tests using synthetic sysfs trees (`crates/tuf-core/tests/safety.rs`) or isolated private `dbus-daemon` instances (`crates/tuf-core/src/dbus/tests.rs`) — never write to live `/sys` or `/dev/i2c-*` inside `cargo test`.
3. **Preserve dGPU runtime power savings:** Telemetry and graphics detection must read sysfs (`/sys/class/drm`, `/sys/class/hwmon`) without spawning tools that force a suspended discrete GPU awake.
4. **Keep Slint layouts responsive at minimum width (`500px`):** Avoid hard-coded wide pixel columns or button overflows; verify narrow (`500px`) and default (`580px`) layouts render without clipped labels or overlapping borders.
5. **Respect DDC/CI VCP timings & error codes:** Monitor I2C transactions require proper settle delays and result-code (`reply[3] == 0x00`) checks so unsupported features fall back cleanly instead of writing bogus values.
