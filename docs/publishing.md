# Releases

The main package is `plavno`. GitHub Release archives bundle the player and the
separate `plavno-spotify` executable. The archive version
follows the player; the helper Cargo package retains its own version and lockfile.

## Binary distribution

The [release workflow](../.github/workflows/release.yml) produces five archives:

| Platform | Runner / build environment | Format |
|---|---|---|
| Linux x86_64 | Ubuntu 24.04 runner, Ubuntu 22.04 container | tar.gz |
| Linux ARM64 | Ubuntu 24.04 ARM runner, Ubuntu 22.04 container | tar.gz |
| macOS Apple Silicon | macOS 15 | tar.gz |
| macOS Intel | macOS 15 Intel | tar.gz |
| Windows x64 | Windows Server 2025, MSVC, static C runtime | zip |

Every build uses `--release --locked` for both Cargo manifests, packages
the two binaries with README and license, extracts the archive, and checks
the player CLI and the Spotify helper's JSON protocol. Protocol checks use an empty
request and require a local error response; they need no credentials or network.
Linux builds inspect the glibc requirements of **both** executables and fail
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

We keep the existing workflow for native builds, archive checks, and publishing.
Linux distribution uses archives with documented prerequisites; no Linux installer
is needed. WinGet is deferred. Stable releases also update the Homebrew tap below.

We evaluated cargo-dist 0.33.0 with `dist plan` in a separate checkout. Importing
the three Cargo projects normally selects only the player for tag `v0.3.0`, since
the helpers are independently versioned `0.1.0` applications. A generic package
declaring all three binaries produces the desired five archive plans and a
Homebrew formula. This is supported, but needs a custom build command and a
distribution version kept in sync with Cargo.toml; ABI and protocol smoke checks
still need our own steps. The plan was evaluated without compiling or publishing.

Cargo-dist would provide generated Homebrew formulae and automatic tap updates.
We implement these in the existing workflow and keep cargo-dist out of the release
toolchain.
See [multi-package behavior](https://axodotdev.github.io/cargo-dist/book/workspaces/workspace-guide.html),
[custom builds](https://axodotdev.github.io/cargo-dist/book/custom-builds.html), and
[Homebrew support](https://axodotdev.github.io/cargo-dist/book/installers/homebrew.html).

## Homebrew tap

The public tap is [gmelikov/homebrew-plavno](https://github.com/gmelikov/homebrew-plavno).
Starting with v0.3.1, users install with:

```sh
brew install gmelikov/plavno/plavno
```

The formula installs prebuilt Apple Silicon or Intel executables: the player and
the Spotify helper. Rust is not required. yt-dlp is installed separately for YouTube. ffmpeg remains optional for HLS; browser-based
sign-in needs Chrome/Chromium. The old v0.3.0 release contains only Linux binaries,
so it cannot populate the tap. v0.3.1 is the first release that publishes the formula.

After publishing a stable GitHub Release, `release.yml` calls
[homebrew.yml](../.github/workflows/homebrew.yml). It generates the formula from
`SHA256SUMS`, checks that the public release contains both macOS archives and
matching checksums, and runs `brew install` and `brew test` on Apple Silicon and
Intel. The tests serve the CI archives over loopback HTTP, keeping their hashes
and formula installation logic unchanged. No service credentials are needed.
Only then does CI commit `Formula/plavno.rb` to the tap. Prerelease/build-metadata
tags do not update Homebrew; old release runs cannot downgrade an existing formula.
Unchanged formulae do not create commits. The tap has its own CI against public
downloads for both architectures.

Publication uses the `HOMEBREW_TAP_DEPLOY_KEY` Actions secret in `gmelikov/plavno`.
Its public key is a write-enabled deploy key on **only** `gmelikov/homebrew-plavno`;
the workflow's GitHub token needs read access only. To rotate it, replace that
deploy key and the secret with a newly generated pair. Never commit the private key.

Test generation, installation, and SSH publishing access against an existing
Release workflow run without changing the tap or publishing a GitHub Release:

```sh
gh workflow run homebrew.yml --ref master \
  -f tag=v0.3.0 -f run_id=38889917612 -f publish=false
```

The normal Release workflow also performs these checks for stable preview builds.
To retry just the Homebrew update after an actual release (use its run ID):

```sh
gh workflow run homebrew.yml --ref master \
  -f tag=v0.3.1 -f run_id=RELEASE_RUN_ID -f publish=true
```

`publish=true` requires existing public release assets and matching checksums;
preview archives alone cannot be published to the tap.

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
