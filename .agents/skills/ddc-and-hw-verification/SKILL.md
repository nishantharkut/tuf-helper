---
name: ddc-and-hw-verification
description: Safe read-only verification workflow for ASUS asusd D-Bus objects, VESA DDC/CI monitors, and Slint UI layout renders in TUF-Helper.
---

# TUF-Helper Safe Hardware & UI Verification

1. **Offline & Private D-Bus Suite:**
   ```sh
   RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
   cargo test --workspace --all-targets --locked
   cargo doc --workspace --no-deps --locked
   ```
   All unit/integration tests use synthetic sysfs fixtures and private `dbus-daemon` buses without touching host hardware.

2. **Read-Only Live Hardware Introspection:**
   - ASUS telemetry & Armoury snapshot: `cargo run --locked -p tuf-core --example test_hw`
   - `asusd` D-Bus properties: `busctl introspect xyz.ljones.Asusd /xyz/ljones`
   - Monitor MCCS capability string: `ddcutil --bus <N> capabilities`

3. **Desktop Packaging & Metadata:**
   ```sh
   bash -n scripts/install.sh
   desktop-file-validate packaging/tuf-helper.desktop
   appstreamcli validate --no-net packaging/io.github.nishantharkut.TufHelper.metainfo.xml
   ```
