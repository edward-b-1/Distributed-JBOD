# Releasing

A release is a tag `vX.Y.Z` on `main`. Pushing the tag runs
`.github/workflows/release.yml`, which builds and publishes everything;
the steps by hand are only the version, the changelog and the tag.

## What a version number says

Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html),
and the workspace `version` in `Cargo.toml` is the only place the number is
written: every crate inherits it and the Python package reads it through
maturin. While the major version is 0:

- a **minor** bump (0.2.x to 0.3.0) says the release is incompatible with
  the one before in something a running cluster depends on: the on-disk
  format, the protocol, the cluster document, or the command line's output
  and exit codes;
- a **patch** bump says anything else.

A build identifies itself as `<version>+<commit>`. Between releases `main`
keeps the last release's number, so a build from `main` reads, say,
`0.2.21+3c3fd58a1`, and the commit says which.

## Every pull request

A pull request adds its entries to `CHANGELOG.md` under `Unreleased`, under
`Added`, `Changed`, `Fixed` or `Removed`, citing its issue or its own
number and the SPEC sections it touches. It does not change the version.

CI (`.github/workflows/ci.yml`) must pass: formatting, clippy, the Rust
tests, the Python client's tests, the PKI helper's tests, and
`THIRD-PARTY-NOTICES` against `Cargo.lock`.

## Cutting a release

1. **The release pull request.** On a branch from `main`:
   - set `version` under `[workspace.package]` in `Cargo.toml`, and run
     `cargo update --workspace` so `Cargo.lock` follows for the workspace
     members alone;
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - <date>`,
     start a new empty `## [Unreleased]` above it, and at the bottom point
     `[Unreleased]` at `compare/vX.Y.Z...HEAD` and add
     `[X.Y.Z]: https://github.com/edward-b-1/Distributed-JBOD/compare/v<previous>...vX.Y.Z`;
   - say at the top of the section what an operator must know before
     upgrading: whether nodes of the previous release can stay in the
     cluster during a rolling upgrade, and anything on disk that must be
     rewritten, as 0.2.20 required of records without a revision.

   Merge it once CI passes.

2. **The tag**, on the merge commit, from an up-to-date `main`:

   ```sh
   git switch main && git pull
   git tag -a vX.Y.Z -m "Version X.Y.Z"
   git push origin vX.Y.Z
   ```

3. **The workflow.** Watch it under Actions, or `gh run watch`. It:
   - refuses the tag unless it is `vX.Y.Z`, the workspace version is
     `X.Y.Z`, `CHANGELOG.md` has an `[X.Y.Z]` section, and the commit is on
     `main`;
   - builds `djbod-node`, `djbod`, `djbod-recover` and `djbod-ui` for
     x86_64 and aarch64 Linux against Debian 12's glibc, packaged with
     `LICENSE`, `THIRD-PARTY-NOTICES` and the README as
     `djbod-X.Y.Z-<arch>-linux.tar.gz`;
   - builds the Python wheel for each (abi3, every Python from 3.10,
     manylinux), with the notices inside the package;
   - builds the Docker image natively on each platform and pushes
     `ghcr.io/edward-b-1/distributed-jbod:X.Y.Z` and `:latest` for both;
   - creates the GitHub release `X.Y.Z` with the changelog section as its
     notes and the archives, the wheels and `SHA256SUMS` attached.

   Every artifact reports `X.Y.Z+<commit>`.

4. **The first release only**: GHCR creates the package private. Make it
   public once under the package's settings, Danger Zone, Change
   visibility.

## When the workflow fails

If it failed before the release was created, nothing was published but
possibly the Docker image's per-platform tags. Fix the cause on `main`
through a pull request, then move the tag to the new commit and push it
again:

```sh
git tag -d vX.Y.Z && git push origin :refs/tags/vX.Y.Z
git tag -a vX.Y.Z -m "Version X.Y.Z" && git push origin vX.Y.Z
```

Once the release exists, never move its tag: fix forward with X.Y.(Z+1).

A pull request that changes the release workflow, the Dockerfile or the
Python packaging runs the whole workflow except the publishing, so a
broken build shows up before a tag does.

## Not yet

- Publishing the wheel to PyPI (#169); for now it is a release asset,
  installed with `pip install <url of the wheel>`.
- Binaries for macOS or other targets.
