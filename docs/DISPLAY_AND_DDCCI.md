# Dual Display & VESA DDC/CI Guide

TUF-Helper manages **built-in laptop displays** and **multiple external DDC/CI monitors** side-by-side so multi-display setups on Wayland work without touching monitor OSD buttons.

---

## How Multi-Display Control Works

`DisplayClient` (`crates/tuf-core/src/dbus/display.rs`) splits control between two native Linux paths:

1. **Internal Laptop Panel (e.g., `eDP-2`):**
   - **Discovery & Refresh Rate:** Queries `org.gnome.Mutter.DisplayConfig` (`GetCurrentState`), identifies the active built-in connector (`is-builtin` / `eDP-` / `LVDS-`), extracts valid modes at the active resolution and scale, and filters duplicate rates. Supports optional opt-in AC/Battery refresh rate switching (`auto_refresh`) when unmirrored.
   - **Backlight Brightness:** Reads and writes normalized brightness (`5–100%`) through Mutter's `SetBacklight` D-Bus interface (and `Backlight` property), keeping the panel above `5%`.

2. **External Monitors (e.g., `HDMI-1`, `DP-1`):**
   - **Multi-Monitor Discovery & Routing:** `DualPlan::externals` enumerates every active non-builtin display into `DisplaySnapshot::external_displays` (`Vec<ExternalDisplaySnapshot>`). When more than one external monitor is connected (`external_display_count > 1`), the **Target External Monitor** selector (disambiguating identical model names by connector ID) routes brightness (`0x10`), contrast (`0x12`), color presets (`0x14`), RGB gain (`0x16/0x18/0x1A`), input source (`0x60`), display scaling (`0x86`), and standby (`0xD6`) to the selected connector (`DisplayClient::*_for`).
   - **Refresh Rate & Orientation:** Tracks Wayland mirror mode (`mirrored`), switches refresh rates independently in Extended mode (blocking per-output changes while mirrored), and applies Wayland transforms (`0`, `90°`, `180°`, `270°`) with contiguous layout origin adjustment via `ApplyMonitorsConfig`.

---

## Multi-Monitor `/dev/i2c-*` Bus Targeting & Strict EDID Validation

Opening the correct I2C bus for each connected external display avoids writing to internal panels, motherboard SMBus controllers, or the wrong monitor when identical models are attached:

1. **DRM Connector-First Priority (`drm_bus_for_connector`):**
   - Maps Mutter connector names (`HDMI-N` to `HDMI-A-N`, `DVI-N` to `DVI-D/I/A-N`) to `/sys/class/drm/card*-<suffix>/ddc` symlinks to try the exact kernel-bound `/dev/i2c-*` adapter first.
   - Falls back to scanning `candidate_i2c_buses()` (NVIDIA adapters first, then `amdgpu`, `i915`, `nouveau`, `radeon`, and `/drm/` paths, explicitly skipping `-eDP-`, `-LVDS-`, and motherboard SMBus/EC buses).
2. **Strict EDID Validation (`0x50`):**
   - Reads the 128-byte EDID at I2C slave `0x50` and enforces `edid_matches_hint`: vendor must match (case-insensitive), model must match, and serial number must match whenever non-empty — distinguishing identical monitor models with different serials.
   - `DDC_CACHE` caches `(connector, bus, hint, EdidSummary)` per monitor, re-verifies the live `0x50` EDID (`cached_edid_valid`) before every reuse, and expires negative unavailable hints after `30 s`.
3. **Cache Isolation Across Fast Polls:**
   - `DisplaySelectionState` in `crates/tuf-gui/src/hardware.rs` preserves cached DDC register values across fast `Poll` refreshes (`snapshot(false)`) only when connector `id` and EDID (`vendor`, `model`, `serial`) match; hot-swapping a different monitor on the same port clears stale DDC fields immediately until the next `Full` probe.

---

## VESA MCCS `0xF3` Capabilities Discovery & Per-EDID Cache

Rather than hard-coding OSD menus for a single monitor model, `DdcDevice::get_capabilities()` reads the monitor's VESA MCCS capabilities string dynamically:

- **Chunked `0xF3` / `0xE3` Protocol (`get_capabilities_i2c`):**
  - Sends Capabilities Request `[0x51, 0x83, 0xF3, offset_hi, offset_lo, chk]` over I2C slave `0x37` starting at `offset = 0`, retrying up to 4 times per chunk with `CAPABILITIES_SETTLE_MS = 45 ms` settle delays.
  - Parses `0xE3` Capabilities Reply frames (`payload_len + 3`, XOR checksum `0x50 ^ ...`, `resp_offset == offset`), recovering from leading non-`0x6E` bytes and NUL padding.
  - Bounds transfers to `MAX_CAPABILITIES_READS = 32` reads so non-compliant firmware cannot loop indefinitely, stops cleanly on empty EOF chunks, and accepts `NullMessage`/read errors if already received chunks have balanced parentheses (`has_balanced_parens`).
  - Falls back to `ddcutil [--bus N | --sn/--mfg/--model] capabilities --terse` / `--verbose` (`Unparsed capabilities string:`) if direct I2C fails.
- **Capabilities Parser (`parse_mccs_capabilities`):**
  - Extracts the `vcp(...)` section into `MonitorCapabilities` (`supported_vcp`, `color_presets` for `0x14`, `input_sources` for `0x60` including `0x1B = USB-C`, `scaling_modes` for `0x86`, and `supports_audio` from `0x62`), deduplicating repeated VCP codes reported by firmware.
- **Per-EDID Cache (`CAPABILITIES_CACHE`):**
  - Caches parsed `MonitorCapabilities` keyed by `(vendor, model, serial)` so repeated `snapshot(true)` refreshes do not re-run slow chunked `0xF3` reads for an already-discovered monitor.

---

## Protocol Hardening, EEPROM Deduplication & Safety Guards

Real external monitors over `/dev/i2c-*` reject fast back-to-back I2C polls or malformed VCP assumptions:

- **Strict DDC/CI Timing & Retries:** Every `Get VCP` sleeps `50 ms` after transmission and retries up to 4 times on DDC Null Messages (`0x6E 0x80 0xBE`) or bus contention. `Set VCP` enforces a `60 ms` settle delay before subsequent reads.
- **Result Code & Checksum Verification:** `parse_vcp_reply` checks payload length (`0x88`), opcode (`0x02`), XOR checksum (`0x50 ^ reply[..10]`), and **VCP result code `reply[3] == 0x00`**. Unsupported features (`rc != 0`) return `VcpError::Unsupported` rather than emitting bogus zero values.
- **EEPROM Write Deduplication:** Both direct I2C (`set_scaled_vcp_if_needed`, `set_raw_vcp_if_needed`, `set_rgb_gain_on`, `set_color_preset`, `set_volume`, `set_mute`) and `ddcutil` CLI paths (`set_ddc_percent_for`, `set_ddc_raw_cli_for`, parsing both `C` and `SNC` hex formats in `--terse`) read the target VCP register first and skip the write if `current == target`, avoiding unnecessary monitor NVRAM/EEPROM wear. If `get_vcp` returns `VcpError::Unsupported`, the write aborts with an explicit error.
- **Color Presets (`VCP 0x14`) & RGB Gain:**
  - When MCCS `0xF3` reports `0x14` options, the UI renders dynamic buttons for every advertised preset (otherwise falling back to Standard `0x06`/`0x05`, 6500K Warm `0x05`, sRGB `0x01`, 9300K Cool `0x08`, User Custom `0x0B`).
  - Automatically checks and switches `VCP 0x14` to `0x0B` (User 1) before writing individual Red (`0x16`), Green (`0x18`), and Blue (`0x1A`) channel gains.
- **Two-Step Hardware Guards (`VCP 0x60` & `VCP 0xD6`):**
  - **Input Source (`VCP 0x60`):** Selecting a new input requires clicking **Switch Input** and then **Confirm Switch** within a 4-second `Timer` (`input_armed`). Switching monitors, changing tabs, or changing the selected item disarms the confirmation to prevent accidental display blackouts.
  - **Display Standby (`VCP 0xD6 = 0x04`):** Clicking **Put to Sleep** arms a 4-second **Confirm Standby** confirmation (`sleep_armed`) before writing `0x04` (DPM Off / Standby).
- **Audio Routing (`wpctl` Fallback):** When `VCP 0x62` returns `VcpError::Unsupported`, `DisplayClient` caches `ddc_audio_supported = false` per EDID and routes volume and mute controls through PipeWire (`wpctl get-volume` / `set-volume` / `set-mute @DEFAULT_AUDIO_SINK@`).

### Implemented VCP Register Map

| VCP Code | Feature | UI Control / Range | Implementation Notes |
| --- | --- | --- | --- |
| `0x10` | Luminance (Backlight) | `0–100%` | Scaled against `VcpValue.max`; EEPROM-deduped |
| `0x12` | Contrast | `0–100%` | Scaled against `VcpValue.max`; EEPROM-deduped |
| `0x14` | Select Color Preset | MCCS `0xF3` presets or 5 fallbacks | Standard `0x06`/`0x05`, `0x05`, `0x01`, `0x08`, `0x0B` |
| `0x16 / 0x18 / 0x1A` | Red / Green / Blue Gain | `0–100%` | Ensures `0x14 == 0x0B` beforehand; EEPROM-deduped |
| `0x60` | Input Source | MCCS `0xF3` inputs | Supports VGA/DVI/DP/HDMI/USB-C (`0x1B`); 4s two-step guard |
| `0x62 / 0x8D` | Audio Volume / Mute | `0–100%`, Mute | Falls back to PipeWire `wpctl` if unsupported (`rc != 0`) |
| `0x86` | Display Scaling | MCCS `0xF3` scaling modes | Aspect, no-scaling, and stretch modes (`0x01–0x0A`) |
| `0xD6` | Power Mode (DPMS) | Standby (`0x04`) | Direct hardware sleep trigger; 4s two-step guard |
| `0xF3 / 0xE3` | Capabilities Request / Reply | Internal discovery | Chunked offset read (`45 ms` settle, max 32 chunks), EDID-cached |

---

## Setup for Non-Root DDC/CI Access

External monitors require user read/write access to `/dev/i2c-*` devices exposed by the DRM GPU driver.

1. **Verify `i2c-dev` kernel module:**
   ```sh
   lsmod | grep i2c_dev || sudo modprobe i2c-dev
   ```
   To load automatically on boot:
   ```sh
   echo i2c-dev | sudo tee /etc/modules-load.d/i2c-dev.conf
   ```

2. **Install `ddcutil` (recommended for udev ACL rules and diagnostics):**
   ```sh
   sudo apt install ddcutil
   ddcutil detect
   ```
   On Ubuntu/Debian, the `ddcutil` package ships `/usr/lib/udev/rules.d/60-ddcutil-i2c.rules`, granting the logged-in desktop user session ACL access (`uaccess`) to monitor `/dev/i2c-*` nodes without adding your user to root/privileged groups. Replug your display cable or run:
   ```sh
   sudo udevadm trigger --subsystem-match=i2c-dev
   ```

3. **Monitor OSD Checklist:**
   - Ensure **DDC/CI** is set to **On** inside the external monitor's physical OSD settings menu.
   - Check `/dev/i2c-*` ACLs with `getfacl /dev/i2c-*` if external monitor brightness stays `Unavailable`.
   - When using **Mirror** mode in GNOME Display Settings, refresh rates are locked by the compositor (`Locked` notice shown in TUF-Helper); switch to **Join Displays (Extended)** to adjust laptop (`eDP`) and external monitor (`HDMI`/`DP`) refresh rates independently or enable AC/Battery auto-refresh.
