{
  lib,
  buildRustPackage,
  version ? "0.1.0",
}:

buildRustPackage {
  pname = "poros";
  inherit version;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;

  # build.rs reads this at build time and embeds it as POROS_VERSION.
  env = {
    POROS_VERSION = version;
  };

  # Nothing is wrapped: Poros talks to tailscaled's LocalAPI socket, and only
  # falls back to a `tailscale` CLI on PATH where no socket exists.
  meta = {
    description = "Expose local development servers privately over Tailscale";
    homepage = "https://github.com/gildrb/poros";
    license = lib.licenses.mit;
    mainProgram = "poros";
    platforms = lib.platforms.unix;
  };
}
