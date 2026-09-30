# Contributing to Tusk

Thanks for helping! Bug reports, ideas and pull requests are all welcome.

## Questions and ideas

- **A question, or an idea you want to talk through:** start a [discussion](https://github.com/alpcanaydin/tusk/discussions).
- **A bug:** open an [issue](https://github.com/alpcanaydin/tusk/issues/new/choose) with your OS version, the Tusk version and the database engine.
- **A security problem:** don't open an issue. See [SECURITY.md](SECURITY.md).

## Development setup

Tusk builds on all three platforms:

- **macOS:** macOS 14 or later on Apple Silicon, with the Xcode Command Line Tools.
- **Windows:** Windows 10 or 11 with the [build tools in the README](README.md#windows-build).
- **Linux:** the [system dependencies in the README](README.md#linux-build).

Docker provides the sample database. `rust-toolchain.toml` pins the Rust
version, and rustup installs it on the first build.

```sh
git clone https://github.com/alpcanaydin/tusk.git && cd tusk
docker compose up -d      # PostgreSQL 17 with sample data on localhost:55432
cargo run                 # the app
cargo test                # unit and integration tests (the ones that need Docker skip without it)
```

The lint tools CI runs are pinned in `mise.toml`. Run `mise install` once, and after that you can run the same checks locally:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
mise exec -- actionlint && mise exec -- shellcheck --severity=warning scripts/*.sh
```

## Pull requests

- Keep a pull request to one change, and say in its description what it changes and why.
- The PR title becomes the commit message on `main` (we squash-merge), so write it as a short sentence in the imperative: "Add a CSV export for query results".
- The **gate** check has to pass: format, clippy, tests, the workflow linters and a secret scan.
- For UI changes, add a screenshot or a short recording.

## Releases

Maintainers release with `scripts/tag-release.sh <version>`. See [Releasing](README.md#releasing-maintainers) in the README.

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
