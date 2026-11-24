# Dual Display & VESA DDC/CI Guide

TUF-Helper manages **built-in laptop displays** and **external DDC/CI monitors** side-by-side so multi-display setups on Wayland work without touching monitor OSD buttons.

---

## How Dual-Display Control Works

`DisplayClient` (`crates/tuf-core/src/dbus/display.rs`) splits control between two native Linux paths:

1. **Internal Laptop Panel (e.g., `eDP-2`):**
   - **Discovery & Refresh Rate:** Queries `org.gnome.Mutter.DisplayConfig` (`GetCurrentState`), identifies the active built-in connector (`is_builtin_display` / `eDP` / `LVDS` / `DSI`), extracts valid modes at the active resolution, and filters duplicate rates.
   - **Backlight Brightness:** Reads and writes normalized brightness (`5–100%`) through Mutter's native `org.gnome.Mutter.DisplayConfig.SetBacklight` D-Bus interface (and fallback backlight properties), ensuring the internal panel never goes completely dark by accident.

2. **External Monitor (e.g., `HDMI-1` / `DP-*`):**
   - **Discovery, Refresh Rate & Orientation:** Queries active logical monitors and connectors from `org.gnome.Mutter.DisplayConfig`, tracks mirror mode (`mirrored`), switches refresh rates independently in Extended mode (or guards against invalid per-output mode mismatches while mirrored), and applies Wayland transforms (`0`, `90°`, `180°`, `270°`) with contiguous layout origin repair via `ApplyMonitorsConfig`.
   - **Direct VESA DDC/CI Hardware Registers (`/dev/i2c-*`):**
     - Scans DRM sysfs connectors (`/sys/class/drm`) to associate only external GPU DRM I2C adapters (skipping unrelated motherboard SMBus/Synopsys buses and internal eDP/LVDS buses).
     - Reads the 128-byte EDID at I2C slave `0x50` and matches `Vendor`, `Model`, and `Serial` against Mutter's reported connector metadata.
     - Communicates directly over I2C address `0x37` (`host 0x51 -> monitor`) using Linux `I2C_SLAVE` (`0x0703`).

---

## Protocol Hardening (VCP Framing & Timing)

Real external monitors (verified directly against a **External MCCS Monitor** on `/dev/i2c-*`) reject fast back-to-back I2C polls or malformed VCP assumptions common in naive implementations:

- **Strict DDC/CI Timing & Retries:** Every `Get VCP` sleeps `50 ms` after request transmission and retries up to 4 times on DDC Null Messages (`0x6e 0x80 0xbe`) or bus contention. `Set VCP` enforces a `60 ms` settle delay before subsequent reads.
- **Result Code & Checksum Verification:** `parse_vcp_reply` checks payload length (`0x88`), opcode (`0x02`), XOR checksum (`0x50 ^ reply[..10]`), and **VCP result code `reply[3] == 0x00`**. Monitors without built-in speakers return `rc = 0x01` (Unsupported) on `VCP 0x62` with `current = 0`; treating `rc != 0` as `VcpError::Unsupported` prevents clobbering system audio with `0%`.
- **External MCCS Monitor Color Presets (`VCP 0x14`) & RGB Gain:**
  - Maps presets cleanly to hardware codes verified via DDC (`Standard -> 0x06` with `0x05` fallback, `6500K Warm -> 0x05`, `sRGB -> 0x01`, `9300K Cool -> 0x08`, `User Custom -> 0x0B`).
  - Automatically switches `VCP 0x14` to `0x0B` (User Custom) before writing individual Red (`0x16`), Green (`0x18`), and Blue (`0x1A`) channel gains so panel firmware accepts RGB writes.
- **Audio Routing (`wpctl` Fallback):** When `VCP 0x62` returns `VcpError::Unsupported`, `DisplayClient` caches `ddc_audio_supported = false` and transparently routes volume and mute controls through PipeWire (`wpctl get-volume` / `set-volume` / `set-mute @DEFAULT_AUDIO_SINK@`).
- **Fallback CLI (`ddcutil`):** If direct `/dev/i2c-*` permissions are missing or an adapter fails to open, supported operations fall back to `ddcutil` if installed on `PATH`, while reporting `external_ddc_available = false` honestly whenever neither direct I2C nor `ddcutil` returns VCP values.

### Implemented VCP Register Map

| VCP Code | Feature | UI Control Range | Implementation Notes |
| --- | --- | --- | --- |
| `0x10` | Luminance (Backlight) | `0–100%` | Scaled cleanly against monitor `VcpValue.max` |
| `0x12` | Contrast | `0–100%` | Scaled cleanly against monitor `VcpValue.max` |
| `0x14` | Select Color Preset | 5 Presets | `0x06`, `0x05`, `0x01`, `0x08`, `0x0B` |
| `0x16 / 0x18 / 0x1A` | Red / Green / Blue Gain | `0–100%` | Sets `0x14 = 0x0B` beforehand |
| `0x62 / 0x8D` | Audio Volume / Mute | `0–100%`, Mute | Falls back to PipeWire `wpctl` if unsupported (`rc = 1`) |
| `0xD6` | Power Mode (DPMS) | Standby (`0x04`) | Direct hardware sleep trigger |

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
   - When using **Mirror** mode in GNOME Display Settings, refresh rates are locked by the compositor (`Locked` notice shown in TUF-Helper); switch to **Join Displays (Extended)** to adjust laptop (`eDP`) and external monitor (`HDMI`/`DP`) refresh rates independently.
