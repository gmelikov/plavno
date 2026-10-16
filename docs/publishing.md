# Releases

The main package is `plavno`. GitHub Release archives bundle the player and the
separate `plavno-spotify` and `plavno-youtube` executables. The archive version
follows the player; helper Cargo packages retain their own versions and lockfiles.

## Binary distribution

The [release workflow](../.github/workflows/release.yml) produces five archives:

| Platform | Runner / build environment | Format |
|---|---|---|
| Linux x86_64 | Ubuntu 24.04 runner, Ubuntu 22.04 container | tar.gz |
| Linux ARM64 | Ubuntu 24.04 ARM runner, Ubuntu 22.04 container | tar.gz |
| macOS Apple Silicon | macOS 15 | tar.gz |
| macOS Intel | macOS 15 Intel | tar.gz |
| Windows x64 | Windows Server 2025, MSVC, static C runtime | zip |

Every build uses `--release --locked` for all three Cargo manifests, packages
the three binaries with README and license, extracts the archive, and checks
the player CLI and both helpers' JSON protocol. Protocol checks use an empty
request and require a local error response; they need no credentials or network.
Linux builds inspect the glibc requirements of **all three** executables and fail
if any requires a version newer than 2.35. Runtime prerequisites are listed in
the [README](../README.md#quick-start).

Full CI and archive builds use the same resolved commit SHA. Each archive has a
SHA256 checksum; the final job verifies all five and creates `SHA256SUMS`.
Artifacts are available from the workflow run even when publishing is disabled.

Test a branch without creating or changing a GitHub Release:

```sh
gh workflow run release.yml --ref distribution-ci -f ref=distribution-ci -f publish=false
```

After merging the workflow, use `--ref master`; `-f ref=BRANCH_OR_SHA` selects
the code to build. Manual runs default to `publish=false`.

We keep the existing workflow rather than adopting cargo-dist for this scope.
Cargo-dist supports multiple Cargo projects and Homebrew generation, but bundling
our three independently versioned projects together still needs configuration,
and WinGet generation is not currently supported. With Linux installers excluded,
native builds, archive checks, and publishing fit directly in the current workflow.
Reconsider cargo-dist when adding generated installers or automatic Homebrew
publishing. See its [workspace support](https://axodotdev.github.io/cargo-dist/book/workspaces/structure.html)
and [installer support](https://axodotdev.github.io/cargo-dist/book/installers/index.html).

Homebrew tap and WinGet publication are separate from these binary releases;
this workflow does not create package-manager repositories or submit manifests.

## Trusted Publishing

Configure the publisher for `gmelikov/plavno` on crates.io with workflow filename
`publish.yml`. Leave GitHub Environment empty unless the publishing job also names
that same environment.

The [publishing workflow](../.github/workflows/publish.yml) runs CI for all three packages on Linux, macOS and Windows,
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
runs CI for that tag and creates a GitHub Release with all five archives described
above and `SHA256SUMS`. Publishing requires the commit to be on `master` and to
match the version tag exactly. Alpine/musl and other CPU architectures require
their own builds.

To create a GitHub Release for an existing tag, without republishing the crate:

```sh
gh workflow run release.yml --ref master -f ref=v0.2.0 -f publish=true
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
