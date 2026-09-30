# Security policy

## Reporting a vulnerability

Please **don't** open a public issue. Report it privately instead: [open a security advisory](https://github.com/alpcanaydin/tusk/security/advisories/new). Only the maintainers see it.

Include what you found, how to reproduce it, and the Tusk version. We'll reply within a few days, and we'll credit you in the release notes unless you'd rather we didn't.

## Supported versions

Only the latest release gets security fixes. On macOS Tusk updates itself; on Windows and Linux, download the latest release.

## How Tusk handles your data

- **Passwords** and SSH passphrases are stored only in your system's credential store, never in files: the macOS Keychain, Windows Credential Manager or the Linux Secret Service (GNOME Keyring, KWallet…).
- **Connection profiles and query history** are stored in `~/Library/Application Support/tusk/` on macOS, `%APPDATA%\tusk\` on Windows and `~/.local/share/tusk/` on Linux.
- **No telemetry.** Tusk doesn't send data anywhere on its own.
- **The AI assistant** runs through the agent CLI you choose (Claude, Codex, Gemini CLI…), under that tool's own account. Tusk doesn't let the agent run SQL; it only writes queries into a tab for you to review.
- **Updates** on macOS are signed with an EdDSA key, and Tusk verifies each one before installing it. macOS releases are also signed with a Developer ID and notarized by Apple. Windows builds aren't code-signed yet.
