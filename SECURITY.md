# Security policy

## Reporting a vulnerability

Please **don't** open a public issue. Report it privately instead: [open a security advisory](https://github.com/alpcanaydin/tusk/security/advisories/new). Only the maintainers see it.

Include what you found, how to reproduce it, and the Tusk version. We'll reply within a few days, and we'll credit you in the release notes unless you'd rather we didn't.

## Supported versions

Only the latest release gets security fixes. Tusk updates itself, so most people already have it.

## How Tusk handles your data

- **Passwords** and SSH passphrases are stored only in the macOS Keychain, never in files.
- **Connection profiles and query history** are stored in `~/Library/Application Support/tusk/`.
- **No telemetry.** Tusk doesn't send data anywhere on its own.
- **The AI assistant** runs through the agent CLI you choose (Claude, Codex, Gemini CLI…), under that tool's own account. Tusk doesn't let the agent run SQL; it only writes queries into a tab for you to review.
- **Updates** are signed with an EdDSA key, and Tusk verifies each one before installing it. Every release is also signed with a Developer ID and notarized by Apple.
