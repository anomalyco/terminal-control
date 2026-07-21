# Changesets

`@kitlangton/terminal-control` and its native binary packages are published as one fixed-version group.

For user-facing npm changes, create a changeset with `bun run changeset` and commit the generated
metadata. Release preparation must keep npm manifests, `Cargo.toml`, `Cargo.lock`, and `bun.lock` on
the same version. Follow `docs/releasing.md` for validation, crates.io publication, npm trusted
publishing, and the GitHub release.
