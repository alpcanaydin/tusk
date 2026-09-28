#!/bin/bash
# Write Casks/tusk.rb for $VERSION / $SHA256 into the tap repo and push it.
# Also used locally by scripts/setup-release.sh to create the first cask.
# The cask has no `auto_updates`, so `brew upgrade` updates Tusk too; the app
# itself updates in place through Sparkle.
set -euo pipefail
: "${VERSION:?}" "${SHA256:?}" "${TAP_REPO:?}" "${TAP_TOKEN:?}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone -q "https://x-access-token:${TAP_TOKEN}@github.com/${TAP_REPO}.git" "$work/tap"
mkdir -p "$work/tap/Casks"
cat > "$work/tap/Casks/tusk.rb" <<EOF
cask "tusk" do
  version "$VERSION"
  sha256 "$SHA256"

  url "https://github.com/alpcanaydin/tusk/releases/download/v#{version}/Tusk-#{version}-arm64.dmg"
  name "Tusk"
  desc "Fast, native, keyboard-driven database client"
  homepage "https://github.com/alpcanaydin/tusk"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on arch: :arm64
  depends_on macos: ">= :sonoma"

  app "Tusk.app"

  zap trash: [
    "~/Library/Application Support/tusk",
    "~/Library/Caches/ai.reyz.tusk",
    "~/Library/HTTPStorages/ai.reyz.tusk",
    "~/Library/Preferences/ai.reyz.tusk.plist",
    "~/Library/Saved Application State/ai.reyz.tusk.savedState",
  ]
end
EOF
cd "$work/tap"
git add Casks/tusk.rb
if git diff --cached --quiet; then
  echo "cask: already at $VERSION"
  exit 0
fi
git -c user.name="tusk-release" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -q -m "tusk $VERSION"
git push -q origin HEAD
echo "cask: tusk $VERSION pushed to $TAP_REPO"
