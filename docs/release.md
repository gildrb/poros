# Release

1. Set `version` in `Cargo.toml` to the new `X.Y.Z`; run `cargo check` to update `Cargo.lock`.
2. Commit, then push `main` to both remotes:

```sh
git push origin main  # Forgejo
git push github main  # GitHub
```

3. Tag and push the tag to GitHub:

```sh
git tag porthmeus-vX.Y.Z
git push github porthmeus-vX.Y.Z
```

`.github/workflows/release.yml` then:

- fails if the tag differs from `Cargo.toml`'s version
- builds `porthmeus-X.Y.Z-{linux-x64,linux-arm64,darwin-arm64,darwin-x64}.tar.gz`; each archive holds `porthmeus` and the symlink `p`, and its binary must print `X.Y.Z`
- attaches `SHA256SUMS` and `install.sh`, then publishes the draft as the latest release

`https://porthmeus.com/install` redirects to `releases/latest/download/install.sh`.
