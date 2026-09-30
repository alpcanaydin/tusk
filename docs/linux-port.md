# Linux port plan

Target: a usable Tusk desktop app on x86_64 Linux under Wayland or Xwayland, while
keeping the existing macOS build working. The Linux install bundles SQL
language servers and uses installed PostgreSQL client tools for backup.

## Requirements and checks

1. **Build environment.** Rust from `rust-toolchain.toml`, a C/C++ compiler,
   CMake, `pkg-config`, and the Wayland/Xwayland, font, and Vulkan development
   libraries needed by GPUI. `cargo check --locked` must pass on Linux.
2. **Credentials.** Use a persistent Linux Secret Service store. Saving,
   reopening, and deleting a connection must not put secrets in JSON. A missing
   or locked service must produce a visible error instead of pretending a
   password was saved.
3. **Desktop behavior.** Main window, dialogs, clipboard, file dialogs, themes,
   and keyboard shortcuts must work on Wayland and Xwayland. Primary shortcuts use
   Command on macOS and Control on Linux; displayed shortcut hints must match.
4. **External tools.** Find `pg_dump`, `pg_restore`, and `psql` from Linux
   paths/PATH. Bundle the two SQL language servers at install time. Report
   missing tools clearly.
5. **Distribution.** Provide a Linux launcher and icon, with a documented
   local installation path. Keep macOS signing, bundling, and Sparkle separate.
6. **Regression gate.** Format, lint, unit tests, a clean Linux build, and a
   manual smoke check for launch, SQLite, saved credentials, and PostgreSQL
   when a local test database is available. Add Linux CI once the build works.

## Progress

- [x] Clone and inspect the macOS-only paths and dependencies.
- [x] Install the pinned Rust toolchain locally.
- [x] Make the Linux build pass (`cargo check` on CachyOS).
- [x] Wire persistent Linux credentials and platform shortcuts.
- [x] Verify tool discovery, Wayland/Xwayland launch, Secret Service, and the
  seeded PostgreSQL and SQLite test paths.
- [x] Add Linux install instructions and CI (locally linted).
- [x] Build and install the release binary and desktop launcher; smoke-test the
  installed app on Wayland and through Xwayland in a Wayland session.
- [x] Check GPU rendering on Wayland: the running Tusk process opens DRM render
  devices and its Intel Iris Xe `i915` render-engine counter advances. GPUI Kit
  uses its WGPU renderer for the Wayland window.
- [x] Build the Linux binary on Ubuntu 24.04 (glibc 2.39), package it for
  Ubuntu, Fedora, and Arch, and smoke-test it on Wayland and Xwayland.

The macOS build and runtime still need verification on a Mac after these
changes; this Linux host cannot run that check.
