{
  buildGoModule,
  lib,
  version ? "0.1.0",
}:

buildGoModule {
  pname = "taildev";
  inherit version;
  src = lib.cleanSource ../.;

  vendorHash = null;
  subPackages = [ "cmd/taildev" ];
  checkPhase = ''
    runHook preCheck
    go test ./...
    runHook postCheck
  '';
  ldflags = [
    "-s"
    "-w"
    "-X main.version=${version}"
  ];

  meta = {
    description = "Expose local development servers privately over Tailscale";
    homepage = "https://github.com/gildrb/taildev";
    license = lib.licenses.mit;
    mainProgram = "taildev";
    platforms = lib.platforms.unix;
  };
}
