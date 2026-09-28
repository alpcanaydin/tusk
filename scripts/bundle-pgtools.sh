#!/bin/bash
# Copy pg_dump / pg_restore / psql and every non-system dylib they load into
# <dest>/{bin,lib}, rewriting load paths to @loader_path so they run on a Mac
# without Homebrew or PostgreSQL installed.
#   scripts/bundle-pgtools.sh <dest>      (needs `brew install libpq`)
set -euo pipefail
dest="$1"
src="${TUSK_PGTOOLS_SRC:-$(brew --prefix libpq)/bin}"
mkdir -p "$dest/bin" "$dest/lib"
queue=()
for t in pg_dump pg_restore psql; do
  cp -f "$src/$t" "$dest/bin/$t"
  chmod u+w "$dest/bin/$t"
  queue+=("$dest/bin/$t")
done

# Non-system dependencies of one Mach-O file.
deps() { otool -L "$1" | tail -n +2 | awk '{print $1}' | grep -vE '^(/usr/lib/|/System/|@)' || true; }

while [ ${#queue[@]} -gt 0 ]; do
  f="${queue[0]}"; queue=("${queue[@]:1}")
  for d in $(deps "$f"); do
    name=$(basename "$d")
    if [ ! -f "$dest/lib/$name" ]; then
      cp -fL "$d" "$dest/lib/$name"
      chmod u+w "$dest/lib/$name"
      install_name_tool -id "@loader_path/$name" "$dest/lib/$name" 2>/dev/null
      queue+=("$dest/lib/$name")
    fi
    case "$f" in
      "$dest/bin/"*) install_name_tool -change "$d" "@loader_path/../lib/$name" "$f" 2>/dev/null ;;
      *)             install_name_tool -change "$d" "@loader_path/$name" "$f" 2>/dev/null ;;
    esac
  done
done
# install_name_tool invalidates signatures; re-sign ad hoc (the app signature
# re-signs them with the real identity afterwards).
for f in "$dest"/bin/* "$dest"/lib/*; do codesign --force --sign - "$f" 2>/dev/null; done
echo "bundled $(ls "$dest/bin" | wc -l | tr -d ' ') tools, $(ls "$dest/lib" | wc -l | tr -d ' ') libs"
