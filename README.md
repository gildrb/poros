# Poros

Install
```sh
curl -fsSL https://raw.githubusercontent.com/gildrb/poros/main/install.sh | bash
```

Run a development command. Open its private HTTPS link on your Mac, phone, or tablet.

```sh
poros vp dev
```
or
```sh
p vp dev
```

Run this in your project on the machine doing the development work. Open the
printed link on any device connected to your tailnet and permitted by its access
rules. Your browser stays local; your code, builds, and development server stay
on the development machine.

## Dashboard

```sh
poros dashboard
```

Lists every listening TCP server your user owns, with its Poros HTTPS URL,
working directory, and command. Select a row with ↑/↓ (or j/k) and press `x`
then `y` to stop it; `x` again offers a force-kill. A Poros row stops its whole
session: the dev server and its HTTPS route. `r` refreshes, `q` quits. Piped
output prints the table once. To run a program named `dashboard`, use
`poros -- dashboard`.

## Requirements

- Tailscale installed, signed in, and running on the development machine and
  viewing device.
- MagicDNS and HTTPS certificates enabled for the tailnet. Tailscale may require
  one-time administrator setup. Certificate names are recorded in public
  Certificate Transparency logs; the development site itself remains private.
- Tailnet access rules that allow the selected HTTPS port.
- The project's usual tools and dependencies installed on the development machine.
  
On Linux, an administrator may need to allow your user to configure Tailscale
once:

```sh
sudo tailscale set --operator="$USER"
```

This grants your account Tailscale operator access, not just permission to run
Poros. Poros does not grant this permission itself or run your application as root.

## Install

With Nix installed:

```sh
nix profile install github:gildrb/poros
poros vp dev
```

Or run without installing:

```sh
nix run github:gildrb/poros -- vp dev
```

The flake supports Apple Silicon and Intel macOS and Linux. It also exports
NixOS, nix-darwin, and Home Manager modules that install the package. They do not
configure your Tailscale account or grant operator permissions.

### Add to a Nix configuration

Add Poros to your flake inputs:

```nix
inputs.poros = {
  url = "github:gildrb/poros";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Include `poros` in your flake's `outputs` arguments. Then add the matching module
to the configuration's `modules` list:

```nix
# nixpkgs.lib.nixosSystem
modules = [ poros.nixosModules.default ];

# nix-darwin.lib.darwinSystem
modules = [ poros.darwinModules.default ];

# home-manager.lib.homeManagerConfiguration
modules = [ poros.homeManagerModules.default ];
```

Alternatively, select the package directly in a module that receives `pkgs`:

```nix
# NixOS or nix-darwin
environment.systemPackages = [
  poros.packages.${pkgs.stdenv.hostPlatform.system}.default
];

# Home Manager: use home.packages instead of environment.systemPackages.
```

Use either the install module or the package list; you do not need both. The
flake also exports `poros.overlays.default` for configurations that prefer
`pkgs.poros`.

## Build locally

```sh
nix build
./result/bin/poros vp dev
```

Or, with Rust 1.80 or newer:

```sh
cargo install --path .
poros vp dev
```

On Linux, process discovery reads /proc and the kernel's socket diagnostics
directly. On macOS it uses the system `ps`, `lsof`, and `netstat`.

Tailscale remains an independently installed, authenticated host service. Poros
talks to tailscaled's local socket when it has one (Linux, and the open-source
macOS daemon), so no extra Tailscale process runs. Otherwise, as with the macOS
app, it runs the `tailscale` CLI; `--tailscale-cli` or `TAILSCALE_CLI` selects
the CLI explicitly.

## Lifetime and security

Poros uses a foreground Tailscale Serve session, never Funnel. The development
server and Poros bridge listen on loopback. Only Tailscale exposes the HTTPS
endpoint, under your existing tailnet access rules. Poros does not open a LAN
listener, modify firewall rules, or publish the site to the internet.

Each invocation owns a separate HTTPS port. Existing Serve routes are not
replaced. Normal command exit or Ctrl-C stops the child processes and removes
only that invocation's sharing session. Poros never runs `tailscale serve reset`.

Run Poros inside a persistent server terminal if it should survive your SSH
session disconnecting. Poros is not a daemon or process-session manager. A hard
kill of Poros can leave its child processes alive; do not treat it as a sandbox.
Over tailscaled's socket, its HTTPS route is still removed when Poros dies.
Commands that detach into separate process groups are not supported.

Browser Host and Origin checks protect the proxy boundary. After they pass,
Poros forwards requests and WebSocket upgrades with the local Host, so dev
servers that reject foreign hosts, like Vite, need no configuration. Keep
application secrets out of frontend bundles: private network access does not
make browser code secret from authorized viewers.