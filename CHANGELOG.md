# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **CLI & `local.TufHelper` D-Bus Shortcut Actions (`--toggle`, `--cycle-profile`, `--step-monitor-brightness <DELTA>`):** Supports GNOME custom keyboard shortcuts by invoking `Toggle`, `CycleProfile`, and `StepMonitorBrightness` over the session bus when running (`execute_blocking` with `12 s` worker timeout), falling back directly to `tuf-core` only when no instance is active.
- **Reactive D-Bus Signal Pipeline (`signals.rs`):** Subscribes to `asusd` (`/xyz/ljones` `PropertiesChanged`), `UPower` (`/org/freedesktop/UPower` `PropertiesChanged`), GNOME Mutter (`MonitorsChanged` and `/org/gnome/Mutter/DisplayConfig` `PropertiesChanged`), and `org.freedesktop.portal.Settings` (`SettingChanged`), debouncing signal bursts (`180 ms` window, `500 ms` max cap) and reconnecting with `500 ms` to `10 s` exponential backoff.
- **Yaru Light Theme & Portal Sync:** Added full Yaru Light tokens alongside Yaru Dark in `Yaru.dark-mode` synchronized with `Palette.color-scheme` and `org.freedesktop.appearance` `color-scheme` (`ReadOne` with `Read` fallback and live `SettingChanged` listener), preserving manual in-session theme toggles via `ThemeSync`.
- **One-Shot 100% Charge & AC/Battery Automation:** Added `xyz.ljones.Platform.OneShotFullCharge` (`Charge to 100% Once`), AC/Battery platform automation (`PlatformAutomationState`: `ChangePlatformProfileOnAc`, `ChangePlatformProfileOnBattery`, `PlatformProfileOnAc`, `PlatformProfileOnBattery`, `DisableNvidiaPowerdOnBattery`, `PlatformProfileLinkedEpp`), and opt-in AC/Battery laptop refresh rate switching (`auto_refresh` persisted atomically in `${XDG_CONFIG_HOME:-$HOME/.config}/tuf-helper/auto_refresh`, defaulting to `false`).
- **Extended Armoury Power & Boost Limits:** Added bounded read/write support for `ppt_pl2_sppt` (Short-Term CPU Boost), `ppt_pl3_fppt` (Peak CPU Boost), and `nv_dynamic_boost` (NVIDIA Dynamic Boost) under `MinValue..=MaxValue.min(DefaultValue)`.
- **Interactive 8-Point Fan Curve Editor:** Added `FanCurveGrid` (`2x4` `P1`–`P8` steppers for CPU and GPU fans, `30–105°C` and `0–100%` monotonic clamping) backed by `get_fan_curves`, `set_custom_fan_curves`, and `reset_fan_curves_to_defaults` with atomic staging, readback verification, and BIOS `[2, 2]` rollback.
- **Multi-Monitor & VESA MCCS `0xF3` Capabilities:** Added multi-external display discovery (`DisplaySnapshot::external_displays`), DRM `/sys/class/drm/*/ddc` bus prioritization, UI monitor selector, chunked `0xF3`/`0xE3` capabilities discovery with per-EDID caching (`CAPABILITIES_CACHE`), dynamic `VCP 0x14` color presets, `VCP 0x60` input source switching (`0x1B` USB-C and standard inputs) with a 4-second two-step confirmation guard, and `VCP 0x86` display scaling control.
- **Packaging & Release Workflows:** Added `[package.metadata.deb]` for `cargo-deb`, AppStream metadata (`packaging/io.github.nishantharkut.TufHelper.metainfo.xml`), and `.github/workflows/release.yml` publishing `.deb`, portable `.tar.gz`, and `SHA256SUMS` to GitHub Releases.

### Changed
- **Worker Concurrency (`HardwareQueue`):** Replaced `sync_channel(3)` with a `Mutex<QueueState>` + `Condvar` queue separating bounded user/shortcut actions (`MAX_QUEUED_ACTIONS = 16`) from coalesced `Fast` (`Poll`) vs `Full` refreshes, preventing fast polls from stacking behind slow DDC scans and keeping hidden `--minimized` instances idle until an error or enabled `UPower` transition occurs.
- **DDC/CI Reliability & Guards:** Added EDID validation on cached `/dev/i2c-*` bus lookups (`cached_edid_valid` + strict serial/model/vendor matching), EEPROM write deduplication across all direct I2C and `ddcutil` CLI write paths (`get_vcp` check prior to `set_vcp`), and a 4-second two-step confirmation guard for DPMS monitor standby (`VCP 0xD6 = 0x04`).
- **CI Gates (`.github/workflows/ci.yml`):** Added blocking `desktop-file-validate` and `appstreamcli validate --no-net` packaging gates alongside `cargo check`, `cargo test`, `cargo doc`, `cargo build --release`, and `bash -n scripts/install.sh`, plus advisory (`continue-on-error: true`) `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` steps.

### Fixed
- Surfacing hardware errors when the main window is hidden (`--minimized`) now shows the window and invokes a full refresh instead of opening a blank unpopulated window.
- Fast `Poll` refreshes preserve previously read DDC state only when connector ID and EDID (`vendor`, `model`, `serial`) match, preventing stale values from appearing after swapping monitors.

## [0.1.0] - 2026-11-25

### Added
- Native Rust + Slint desktop UI styled with `cosmic-dark` widgets and Yaru Dark visual tokens across six views: **Overview**, **System**, **Cooling**, **Display**, **Lighting**, and **Battery**.
- Simultaneous dual-display brightness and refresh-rate control supporting both internal laptop displays (`eDP` via Mutter `SetBacklight`) and external monitors via direct VESA DDC/CI (`/dev/i2c-*` + EDID matching + `ddcutil` fallback), including Wayland Mirror Mode detection.
- Full external monitor hardware controls: contrast (`VCP 0x12`), Lenovo-verified color presets (`VCP 0x14`), per-channel Red/Green/Blue gain (`0x16/0x18/0x1A` with automatic User Custom selection), DPMS standby (`VCP 0xD6`), Wayland rotation (`ApplyMonitorsConfig`), and automatic PipeWire (`wpctl`) audio fallback when `VCP 0x62` returns unsupported (`rc = 1`).
- Complete ASUS `asusd` integration: platform power profiles, dual fan curve switching with rollback safety, GPU mode queueing (`Eco`/`Hybrid`/`Ultimate`), sustained CPU power limits (`SPL`), NVIDIA thermal limits, Aura keyboard lighting effects & static RGB picker, panel overdrive, and battery charge limits (`20–100%`).
- Rolling 120-second SVG telemetry charts for CPU/AMD GPU temperatures and fan RPMs, without waking a suspended NVIDIA dGPU (`sys/class/drm`).
- Desktop integration: `xdg_app_id("tuf-helper")`, scalable SVG icon, SNI system tray (`--minimized`), XDG autostart management, Mesa EGL selection on AMD iGPUs (`50_mesa.json`), and portable `scripts/install.sh`.
