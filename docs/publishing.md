# Releases

The main package is `plavno`; the first published version is `0.1.0`.
Spotify and YouTube helpers remain separate binaries. Build them from a repository
checkout using the [service setup instructions](services.md#spotify-and-youtube-music).

## Trusted Publishing

Configure the publisher for `gmelikov/plavno` on crates.io with workflow filename
`publish.yml`. Leave GitHub Environment empty unless the publishing job also names
that same environment.

The [publishing workflow](../.github/workflows/publish.yml) runs CI for all three packages on Linux and macOS,
checks that the release commit is on `master`, verifies the package, and obtains
a short-lived token through GitHub OIDC. The token is revoked when the job ends;
no crates.io API token secret is needed in GitHub.

To test packaging and authentication without uploading:

```sh
gh workflow run publish.yml --ref master -f dry_run=true
```

## New releases

1. Increase `package.version` in `Cargo.toml`, refresh `Cargo.lock` with `cargo check`,
   and commit the change.
2. Push `master`, then tag that release commit and push the tag. For example:

   ```sh
   git push origin master
   git tag -a v0.2.0 -m "v0.2.0"
   git push origin v0.2.0
   ```

The tag must exactly match `v` plus the package version. A successful tag run uploads
the verified package to crates.io. The separate [release workflow](../.github/workflows/release.yml)
runs CI for that tag and creates a GitHub Release with Linux x86_64 and ARM64 binary archives
and `SHA256SUMS`. It builds in Ubuntu 22.04 on a current runner and checks that the
binary requires no newer glibc than 2.35. ALSA and D-Bus remain runtime dependencies;
each archive contains the main player, README, and license. Alpine/musl and other CPU
architectures require their own builds.

To create a GitHub Release for an existing tag, without republishing the crate:

```sh
gh workflow run release.yml --ref master -f tag=v0.2.0
```

Manual `publish.yml` runs default to dry run; actual publishing also requires
selecting a matching version tag. Published crate versions are immutable.

Users install the release with `cargo install plavno --locked`. ALSA/D-Bus development
packages are required during installation; see the [README](../README.md#quick-start).

## Manual fallback

From a clean checkout, authenticate with `cargo login`, then run:

```sh
cargo publish --locked --dry-run
cargo publish --locked
```

Inspect archive contents with `cargo package --list`. The archive includes source,
tests, documentation, and the license; build outputs and optional helpers are excluded.

References: [Cargo publishing](https://doc.rust-lang.org/cargo/reference/publishing.html),
[crates.io OIDC action](https://github.com/rust-lang/crates-io-auth-action).
