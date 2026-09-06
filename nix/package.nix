{
  buildGoModule,
  lib,
  makeWrapper,
  lsof,
  procps,
  stdenv,
  version ? "0.1.0",
}:

buildGoModule {
  pname = "poros";
  inherit version;
  src = lib.cleanSource ../.;

  vendorHash = null;
  nativeBuildInputs = [ makeWrapper ];
  nativeCheckInputs = [ lsof ] ++ lib.optional stdenv.hostPlatform.isLinux procps;
  postInstall = ''
    wrapProgram $out/bin/poros --prefix PATH : ${lib.makeBinPath ([ lsof ] ++ lib.optional stdenv.hostPlatform.isLinux procps)}${lib.optionalString stdenv.hostPlatform.isDarwin ":/bin"}
  '';
  subPackages = [ "cmd/poros" ];
  checkPhase = ''
    runHook preCheck
    ${lib.optionalString stdenv.hostPlatform.isDarwin "export PATH=$PATH:/bin POROS_TEST_NO_PROCESS_INSPECTION=1"}
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
    homepage = "https://github.com/gildrb/poros";
    license = lib.licenses.mit;
    mainProgram = "poros";
    platforms = lib.platforms.unix;
  };
}
