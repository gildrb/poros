# taildev

`taildev` gives a localhost development server a predictable, tailnet-only URL.
It discovers the current machine through the local Tailscale client, binds only
to that Tailscale address, and proxies HTTP and WebSocket traffic to localhost.

No account tokens, tailnet names, IP addresses, certificates, or other state
are stored by this project.

## Run without installing

Start a development command on the default port (`5173`):

```bash
nix run github:gildrb/taildev -- -- npm run dev -- --host {host} --port {port}
```

Or choose a port:

```bash
nix run github:gildrb/taildev -- --port 3000 -- python -m http.server {port} --bind {host}
```

`taildev` prints a URL like:

```text
Tailnet URL: http://workstation.example.ts.net:5173/
Local target: http://127.0.0.1:43123
```

The child command receives `PORT`, `HOST`, `TAILDEV_TARGET_HOST`, and
`TAILDEV_URL`. A fresh backend port is selected automatically, keeping the
Tailnet-facing port stable and avoiding bind conflicts. Commands can use
`{host}`, `{port}`, and `{url}` placeholders when their server does not read
those environment variables. The proxy supports hot-reload WebSockets.
Wrapped commands run non-interactively so their complete process group can be
stopped reliably; use the `taildev` terminal for Ctrl-C, not framework keyboard
shortcuts.

To expose a server that is already running:

```bash
taildev --port 5173 --target http://127.0.0.1:5173
```

Use a different local target when its port differs from the tailnet port:

```bash
taildev --port 8080 --target http://127.0.0.1:3000
```

## Install with Nix

The flake provides packages for Apple Silicon and Intel macOS and Linux, plus
an overlay and modules for NixOS, nix-darwin, and Home Manager.

```nix
{
  inputs.taildev = {
    url = "github:gildrb/taildev";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, taildev, ... }: {
    nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      modules = [ taildev.nixosModules.default ];
    };
  };
}
```

For nix-darwin use `taildev.darwinModules.default`; for Home Manager use
`taildev.homeManagerModules.default`. Overlay users can import
`taildev.overlays.default` and install `pkgs.taildev`.

## Requirements and security

- Tailscale must be installed, signed in, and running locally.
- MagicDNS is used when available; otherwise the Tailscale IP is printed.
- The proxy listens only on the current machine's Tailscale address.
- Requests with a `Host` outside the current node's MagicDNS name, short name,
  or Tailscale IP are rejected.
- Browser requests with a foreign `Origin` are rejected. Accepted origins and
  local absolute redirects are translated across the proxy boundary.
- HTTP is encrypted in transit by Tailscale/WireGuard, but browsers do not
  treat a remote HTTP origin as a secure context. Use an SSH local forward or
  Tailscale Serve when WebGPU, microphone, or another secure-context API is
  required.
