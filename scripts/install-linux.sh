#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo >/dev/null; then
    . "$HOME/.cargo/env"
fi
cd "$(dirname "$0")/.."
cargo build --locked --release

bin_dir="$HOME/.local/bin"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
install -Dm755 "${CARGO_TARGET_DIR:-target}/release/tusk" "$bin_dir/tusk"
scripts/bundle-pgls.sh "$bin_dir"
scripts/bundle-sqls.sh "$bin_dir"
install -Dm644 assets/icon/tusk-1024.png "$data_dir/icons/hicolor/1024x1024/apps/tusk.png"
install -Dm644 assets/icon/Tusk.icon/Assets/elephant.svg "$data_dir/icons/hicolor/scalable/apps/tusk.svg"
install -Dm644 assets/linux/tusk.desktop "$data_dir/applications/tusk.desktop"
echo "Installed Tusk to $bin_dir/tusk"
