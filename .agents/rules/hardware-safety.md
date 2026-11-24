---
trigger: always_on
---

# TUF-Helper Engineering & Hardware Safety Rules

1. **Zero unsolicited hardware writes on startup:** Launching or refreshing TUF-Helper must remain strictly read-only. Only explicit user actions (or explicitly opted-in `auto_refresh` transitions) may write to `asusd`, Mutter, or `/dev/i2c-*`.
2. **Firmware bounds & readback verification (`tuf-core`):**
   - Clamp writable `AsusArmoury` limits (`spl`, `sppt`, `fppt`, `nv_temp_target`, `nv_dynamic_boost`) strictly to `MinValue..=MaxValue.min(DefaultValue)`.
   - Preserve dual-fan curve atomicity and BIOS `[2, 2]` rollback on partial failure.
   - Verify property writes via `confirmed(...)` readback before reporting success.
3. **Monitor scalar EEPROM & I2C safety (`display.rs`):**
   - Always check current register value (`get_vcp`) and skip `set_vcp` when already equal to target.
   - Only open `/dev/i2c-*` devices associated with GPU DRM connectors, and verify 128-byte EDID (`vendor`, `model`, `serial`) on every cache hit.
   - Keep `4s` two-step confirmation guards on destructive/blackout-prone DDC actions (`VCP 0xD6` Standby and `VCP 0x60` Input Source switch).
4. **dGPU power hygiene:** Pin Mesa EGL (`50_mesa.json`) when an AMD iGPU (`0x1002`) is present and read `/sys/class/drm` rather than waking a D3cold NVIDIA GPU.
5. **UI & verification:** Keep `ui/*.slint` responsive at `500px` in both `Yaru.dark-mode` Dark and Light polarity. Validate changes with `RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked` and `cargo test --workspace --all-targets --locked`.
