# cargo-publish-plz

A minimal release tool for Cargo crates and workspaces:

- `cargo publish-plz update` bumps the versions of crates changed since their last published release;
- `cargo publish-plz publish` publishes every crate whose current version is not in the registry yet.

The version bump logic follows [release-plz](https://github.com/release-plz/release-plz),
without changelogs, git tags, release PRs or any forge (GitHub/GitLab) integration.
All it needs is `cargo`, `git` and HTTP access to the registry index.

## Install

```sh
cargo install cargo-publish-plz
```

`publish` relies on multi-package `cargo publish`, available since Cargo 1.90.

## Typical flow

```sh
cargo publish-plz update      # bump versions, review the diff
git commit -am "chore: release"
cargo publish-plz publish     # publish everything that isn't published yet
```

## `cargo publish-plz update`

For every publishable package (`publish != false`):

1. **Registry status.** The registry index is queried:
   - the crate was never published: nothing to bump, it will be published as is;
   - the local version is not in the registry: the crate was already bumped and is waiting for `publish`;
   - the local version is published: continue.
2. **Change detection.** The published `.crate` is downloaded and compared file by file with what
   `cargo package --list` would pack now, plus `Cargo.toml.orig` against the local `Cargo.toml`.
   **Changes in `*.md` files are ignored by default**; pass `--all` to take them into account.
3. **Bump level.** The commit the crate was published from is read from `.cargo_vcs_info.json`.
   Commits since then that touch the package directory are parsed as
   [Conventional Commits](https://www.conventionalcommits.org/). Without `--all`, commits that only
   change `*.md` files are skipped.

   | Current version | Breaking (`feat!:`, `BREAKING CHANGE:`) | `feat:` | anything else |
   |-----------------|-----------------------------------------|---------|---------------|
   | `0.x.y`         | minor                                   | patch   | patch         |
   | `>= 1.0.0`      | major                                   | minor   | patch         |
   | `x.y.z-pre.N`   | `pre.N+1`                               | `pre.N+1` | `pre.N+1`   |

   If the source commit is unknown (no git, shallow clone), the bump is a patch.
4. **Dependents.** Workspace members depending on a bumped crate get their version requirement
   updated and a patch bump, transitively. This covers `[dependencies]`, `[build-dependencies]`,
   `[target.*.dependencies]` and `[workspace.dependencies]`. A `[dev-dependencies]` requirement is
   updated only when it no longer matches the new version.
5. **Shared version.** Members with `version.workspace = true` are bumped together, using the
   strongest change among them; `[workspace.package] version` is updated.
6. **Write.** Manifests are edited in place, keeping formatting and comments, then
   `cargo update --workspace` refreshes `Cargo.lock`.

Example:

```text
$ cargo publish-plz update
git_cmd: 0.8.0 -> 0.9.0 (breaking: `src/lib.rs` changed, 1 commit)
release_plz_core: 0.38.7 -> 0.38.8 (dependency `git_cmd` updated)
  crates/release_plz_core/Cargo.toml: `git_cmd` 0.8.0 -> 0.9.0
```

Options:

| Option                   | Description                                             |
|--------------------------|---------------------------------------------------------|
| `--all`                  | Count changes in all files, including `*.md`            |
| `--dry-run`              | Print what would change without writing anything        |
| `-p, --package <SPEC>`   | Only check these packages (dependents are still bumped) |
| `--workspace`            | Check all workspace members                             |
| `--manifest-path <PATH>` | Path to `Cargo.toml`                                    |
| `--registry <NAME>`      | Only check packages allowed to go to this registry      |

## `cargo publish-plz publish`

- Packages that are already published at their current version are skipped with a warning, not an error,
  so the command can be safely re-run after a partial failure.
- The rest is published with a single `cargo publish -p a -p b ...` per registry; Cargo orders the
  packages and waits for each one to appear in the index before publishing its dependents.

Options:

| Option                   | Description                                      |
|--------------------------|--------------------------------------------------|
| `--dry-run`              | Perform all checks without uploading             |
| `--allow-dirty`          | Allow uncommitted changes                        |
| `--no-verify`            | Don't build the packages before uploading        |
| `-p, --package <SPEC>`   | Publish only these packages                      |
| `--workspace`            | Publish all workspace members                    |
| `--manifest-path <PATH>` | Path to `Cargo.toml`                             |
| `--registry <NAME>`      | Only publish packages allowed to go to this registry |
| `-- <ARGS>...`           | Extra arguments passed to `cargo publish`        |

## Package selection

Both commands pick packages the same way `cargo publish` does: `-p` and `--workspace` win; otherwise
running inside a member directory selects that member, and running at the workspace root selects all
members. Packages with `publish = false` are always skipped.

## Registries

Each package goes to the registry `cargo publish` would pick for it, honouring `package.publish`
(including `publish.workspace = true`):

| `package.publish`          | without `--registry`                       | with `--registry X`             |
|----------------------------|--------------------------------------------|---------------------------------|
| `false`                    | skipped                                    | skipped                         |
| unset / `true`             | `registry.default`, else crates.io         | `X`                             |
| `["Y"]`                    | `Y`                                        | `X` if `X == Y`, else skipped   |
| `["Y", "Z"]`               | error: pass `--registry`                   | `X` if listed, else skipped     |

Skipped packages are reported with a warning. Packages are grouped by registry and every
`cargo publish` call gets an explicit `--registry`. `update` uses the same rules to choose
the registry it compares against.

Registries other than crates.io are read from Cargo configuration:

```toml
# .cargo/config.toml
[registries.my-registry]
index = "sparse+https://my-registry.example.com/index/"
```

`CARGO_REGISTRIES_<NAME>_INDEX` overrides the config. If the index requires authentication, the token is
taken from `CARGO_REGISTRIES_<NAME>_TOKEN` or `$CARGO_HOME/credentials.toml`.

## Limitations

- Only sparse registries are supported (crates.io or `sparse+...` indexes); git indexes are not.
- Credential providers are not supported for reading private indexes; use a token.
- Changes visible only through workspace inheritance (e.g. a version bump of an external dependency
  in the root `[workspace.dependencies]`) are not detected for members that inherit it.
- Registries are published one after another, so a crate depending on an unpublished workspace
  crate from another registry may need a second run.

## License

[MIT](LICENSE)
