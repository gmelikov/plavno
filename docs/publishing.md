# Publishing to crates.io

The main package is `plavno`. Spotify and YouTube helpers are separate binaries;
publishing the player does not install those helpers. Build them from a repository
checkout using the [service setup instructions](services.md#spotify-and-youtube-music).

## First release

1. Sign in to [crates.io](https://crates.io) with GitHub and verify your email.
2. Create an API token with permission to publish new crates, then enter it at the
   hidden prompt from `cargo login`.
3. From a clean checkout with passing CI, verify the package:

   ```sh
   cargo publish --locked --dry-run
   ```

4. Upload only when ready to release:

   ```sh
   cargo publish --locked
   ```

Cargo verifies the packaged source before uploading. Inspect included files with
`cargo package --list`. Source, tests, documentation, and the license are included;
build artifacts and optional helper packages are excluded.

After publication, users can install the player with `cargo install plavno --locked`.
Linux still requires ALSA/D-Bus development packages during installation; see the
[README](../README.md#quick-start).

## Later releases

Increase `package.version` in `Cargo.toml`, update `Cargo.lock`, run checks, and
commit before repeating the dry run and publish. Published version contents are
immutable; fixes require a new version.

For automated releases, crates.io also supports [Trusted Publishing](https://github.com/rust-lang/crates-io-auth-action):
configure a GitHub repository/workflow as a trusted publisher and exchange an OIDC
identity for a short-lived token. The CI workflow currently runs checks only.

Reference: [Cargo publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html).
