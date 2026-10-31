{
  lib,
  buildRustPackage,
}:

buildRustPackage {
  pname = "porthmeus";
  # One version: Cargo.toml's, which the binary prints and the release tag carries.
  inherit ((lib.importTOML ../Cargo.toml).package) version;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;

  # `p` is the short command, as in the release archives.
  postInstall = ''
    ln -s porthmeus "$out/bin/p"
  '';

  # Nothing is wrapped: Porthmeus talks to tailscaled's LocalAPI socket, and only
  # falls back to a `tailscale` CLI on PATH where no socket exists.
  meta = {
    description = "Expose local development servers privately over Tailscale";
    homepage = "https://porthmeus.com";
    license = lib.licenses.mit;
    mainProgram = "porthmeus";
    platforms = lib.platforms.unix;
  };
}
