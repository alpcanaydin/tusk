{
  runCommand,
  writeText,
  desktop-file-utils,
  glibc,
  jq,
  postgresql_18,
  tusk,
}:

let
  initialize = writeText "tusk-mcp-initialize.json" ''
    {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
  '';
in
runCommand "tusk-nix-runtime-check"
  {
    nativeBuildInputs = [
      desktop-file-utils
      jq
    ];
  }
  ''
    # Exercise the real Tusk executable without a display or a database.
    env -u DISPLAY -u WAYLAND_DISPLAY \
      ${tusk}/bin/tusk --mcp-bridge "$TMPDIR/unused.sock" < ${initialize} \
      | jq -e '.result.serverInfo.name == "tusk" and .result.serverInfo.version == "${tusk.version}"'

    for binary in ${tusk}/bin/.tusk-wrapped ${tusk}/libexec/tusk/*; do
      ${glibc.bin}/bin/ldd "$binary" > dependencies.txt
      if grep -q 'not found' dependencies.txt; then
        cat dependencies.txt
        exit 1
      fi
    done

    ${tusk}/libexec/tusk/postgres-language-server --version
    ${tusk}/libexec/tusk/sqls -version
    for tool in pg_dump pg_restore psql; do
      ${postgresql_18}/bin/"$tool" --version
    done

    desktop-file-validate ${tusk}/share/applications/tusk.desktop
    test -s ${tusk}/share/icons/hicolor/1024x1024/apps/tusk.png
    test -s ${tusk}/share/icons/hicolor/scalable/apps/tusk.svg
    touch "$out"
  ''
