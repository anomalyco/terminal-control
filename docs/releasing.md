# Releasing Terminal Control

Terminal Control releases one aligned version across the public `terminal-control` crate, the
`@kitlangton/terminal-control` client, and four native npm packages. npm packages are assembled and
published by the manual `npm-release.yml` workflow; crates.io publication and the GitHub release are
separate explicit steps.

## Prepare The Version

Prepare releases from an up-to-date `main` after feature CI passes. User-facing changes must already
have Changesets.

1. Run `bun run version-packages`. The fixed npm package group must resolve to one version.
2. Set the same version in `Cargo.toml`, then refresh the `terminal-control` entry in `Cargo.lock`.
3. Update the workspace package versions and native optional-dependency versions recorded in
   `bun.lock`. Verify them explicitly; current Bun versions can preserve stale workspace metadata
   even after `bun install --force`.
4. Verify the Changesets were consumed and the npm changelogs describe the release.
5. Commit these files as `chore: release terminal-control X.Y.Z` and merge that release commit to
   `main` through CI.

Native packaging rejects a Rust executable whose `termctrl --version` differs from its npm package
manifest. Do not bypass that check or publish package formats at different versions.

## Validate The Release

Run the complete local validation from `AGENTS.md`, followed by the publishable crate checks:

```bash
cargo package
cargo publish --dry-run --locked
```

Then assemble and validate all npm artifacts without publishing:

```bash
gh workflow run npm-release.yml --ref main -f publish=false
```

Confirm the workflow run targets the intended release commit. Its matrix builds macOS and Linux
binaries for arm64 and x64, verifies the complete fixed-version tarball set, and installs the packed
client from clean Bun and Node/Vitest consumers.

## Publish

Publishing is an irreversible public release. From the validated release commit:

```bash
cargo publish --locked
gh workflow run npm-release.yml --ref main -f publish=true
```

The npm workflow uses trusted publishing through GitHub Actions OIDC and is retry-safe: it skips an
exact package version already present in npm. crates.io publication requires Cargo credentials for an
owner of `terminal-control`; do not add registry tokens to the repository.

After both registries report the new version, create the matching tag and GitHub release:

```bash
gh release create vX.Y.Z --target main --generate-notes --title "Terminal Control vX.Y.Z"
```

Verify the public artifacts:

```bash
cargo info terminal-control
npm view @kitlangton/terminal-control version
```

Install the released package in a clean consumer and confirm `termctrl --version` reports `X.Y.Z`.

## Trusted Publishing

Each npm package must configure `anomalyco/terminal-control` and workflow `npm-release.yml` as its
trusted publisher. The publish job uses Node 24, current npm, and `id-token: write`; it does not use a
long-lived npm token.
