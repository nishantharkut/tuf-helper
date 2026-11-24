# Security Policy

## Scope & Privilege Model

TUF-Helper runs entirely as an unprivileged desktop user process:

- **ASUS platform, fan curve, Aura, GPU, and battery settings** are handled exclusively over D-Bus via `asusd` (`xyz.ljones.Asusd`), subject to `asusd`'s own polkit/system authorization policies.
- **Internal display brightness, refresh rate, and layout** use GNOME Mutter's session D-Bus API (`org.gnome.Mutter.DisplayConfig`).
- **External monitor DDC/CI** opens only DRM-associated `/dev/i2c-*` character devices permitted to the active desktop user session via kernel `uaccess` udev rules, after matching connector EDID metadata.
- **No elevated `setuid` binaries, background root daemons, network telemetry, or remote updates** are included.

## Reporting a Vulnerability

If you discover a security issue (such as unsafe D-Bus input handling, path traversal, shell command injection, or unintended privilege escalation):

1. **Do not open a public GitHub issue** with exploit details.
2. Report privately via [GitHub Security Advisories](https://github.com/nishantharkut/tuf-helper/security/advisories/new).
3. Include steps to reproduce, affected versions/hardware, and impact assessment.

Reports are reviewed promptly and fixes are validated against the offline safety and isolated `dbus-daemon` test suites prior to disclosure.
