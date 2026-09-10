# Vite+ cross-machine smoke test

Use a disposable copy of this fixture on the development machine. Install its
locked dependencies with `vp install --frozen-lockfile`, then run
`poros vp dev` there. Both `poros` and `vp` must be on that terminal's PATH.
Do not use an existing application or port.

On the viewing machine, install this fixture's dependencies and Playwright
Chromium (`vp exec playwright install chromium` if needed), then run:

```sh
POROS_TEST_URL=https://server.example.ts.net:PORT \
POROS_TEST_SSH=server \
POROS_TEST_ROOT=/absolute/path/to/fixture \
vp run test:e2e
```

`POROS_TEST_SSH` is an existing SSH alias for the development machine. Omit it
when the fixture is local. `POROS_TEST_ROOT` is the fixture directory on the
machine running Vite. The SSH test needs Python 3 there to edit its fixture.

The test verifies trusted HTTPS, a same-origin secure WebSocket, and an edited
module appearing in the browser without reloading the document. It restores
`message.ts` afterward. It does not start or stop Poros or modify Tailscale
configuration; stop your Poros process after testing and verify its route is gone.

Vite+ uses the pinned Node.js version and its default pnpm package manager.
`vite.config.ts` binds the fixture to loopback without fixing its port. Do not
use `--host 0.0.0.0`: Poros discovers loopback listeners, not LAN listeners.
No custom Vite plugin or HMR URL rewrite is required.
