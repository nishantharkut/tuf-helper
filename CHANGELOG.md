# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-25

### Added
- Native Rust + Slint desktop UI styled with `cosmic-dark` widgets and Yaru Dark visual tokens across six views: **Overview**, **System**, **Cooling**, **Display**, **Lighting**, and **Battery**.
- Simultaneous dual-display brightness and refresh-rate control supporting both internal laptop displays (`eDP` via Mutter `SetBacklight`) and external monitors via direct VESA DDC/CI (`/dev/i2c-*` + EDID matching + `ddcutil` fallback), including Wayland Mirror Mode detection.
- Full external monitor hardware controls: contrast (`VCP 0x12`), Lenovo-verified color presets (`VCP 0x14`), per-channel Red/Green/Blue gain (`0x16/0x18/0x1A` with automatic User Custom selection), DPMS standby (`VCP 0xD6`), Wayland rotation (`ApplyMonitorsConfig`), and automatic PipeWire (`wpctl`) audio fallback when `VCP 0x62` returns unsupported (`rc = 1`).
- Complete ASUS `asusd` integration: platform power profiles, dual fan curve switching with rollback safety, GPU mode queueing (`Eco`/`Hybrid`/`Ultimate`), sustained CPU power limits (`SPL`), NVIDIA thermal limits, Aura keyboard lighting effects & static RGB picker, panel overdrive, and battery charge limits (`20–100%`).
- Rolling 120-second SVG telemetry charts for CPU/AMD GPU temperatures and fan RPMs, without waking a suspended NVIDIA dGPU (`sys/class/drm`).
- Desktop integration: `xdg_app_id("tuf-helper")`, scalable SVG icon, SNI system tray (`--minimized`), XDG autostart management, Mesa EGL selection on AMD iGPUs (`50_mesa.json`), and portable `scripts/install.sh`.
