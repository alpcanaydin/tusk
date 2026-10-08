{
  lib,
  stdenvNoCC,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  dpkg,
  makeBinaryWrapper,
  dbus,
  fontconfig,
  libxcb,
  libxkbcommon,
  vulkan-loader,
  wayland,
  postgresql_18,
}:

let
  runtimeLibraries = [
    dbus
    fontconfig
    libxkbcommon
    vulkan-loader
    wayland
  ];
in
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "tusk";
  # Pin a published release: Cargo.toml may already name an unreleased version.
  version = "0.2.1";

  src = fetchurl {
    url = "https://github.com/alpcanaydin/tusk/releases/download/v${finalAttrs.version}/Tusk-${finalAttrs.version}-ubuntu-amd64.deb";
    hash = "sha256-y2zaoqNl7Vpg48ykZGFEqsFyrfgkLSpn2QqGdy/5whc=";
  };

  nativeBuildInputs = [
    autoPatchelfHook
    dpkg
    makeBinaryWrapper
  ];
  buildInputs = runtimeLibraries ++ [
    stdenv.cc.cc.lib
    libxcb
  ];
  runtimeDependencies = map lib.getLib runtimeLibraries;
  strictDeps = true;

  unpackPhase = ''
    runHook preUnpack
    dpkg-deb -x "$src" source
    runHook postUnpack
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    cp -a source/usr/. "$out/"

    # Keep the bundled, patched language servers private to Tusk.
    mkdir -p "$out/libexec/tusk"
    mv "$out/bin/postgres-language-server" "$out/bin/sqls" "$out/libexec/tusk/"
    substituteInPlace "$out/share/applications/tusk.desktop" \
      --replace-fail "Exec=tusk" "Exec=$out/bin/tusk"
    runHook postInstall
  '';

  postFixup = ''
    wrapProgram "$out/bin/tusk" \
      --prefix LD_LIBRARY_PATH : "/run/opengl-driver/lib:${lib.makeLibraryPath runtimeLibraries}" \
      --prefix PATH : "${lib.makeBinPath [ postgresql_18 ]}" \
      --set-default TUSK_PG_BIN "${lib.getBin postgresql_18}/bin" \
      --set-default TUSK_PGLS "$out/libexec/tusk/postgres-language-server" \
      --set-default TUSK_SQLS "$out/libexec/tusk/sqls"
  '';

  meta = {
    description = "Native GPU-rendered database client with bundled SQL language servers";
    homepage = "https://github.com/alpcanaydin/tusk";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    mainProgram = "tusk";
  };
})
