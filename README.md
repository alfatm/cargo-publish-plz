# cargo-publish-plz

A minimal release tool for Cargo crates and workspaces:

- `cargo publish-plz update` bumps the versions of crates changed since their last published release;
- `cargo publish-plz check` fails when a crate changed without a version bump (for CI);
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

In CI, `cargo publish-plz check` on merge requests catches a forgotten `update`.

## `cargo publish-plz update`

For every publishable package (`publish != false`):

1. **Registry status.** The registry index is queried:
   - the crate was never published: nothing to bump, it will be published as is;
   - the local version is not in the registry: the crate was already bumped and is waiting for `publish`;
   - the local version is published: continue.
2. **Change detection.** The published `.crate` is downloaded and compared with the local package:
   - file by file with what `cargo package --list` would pack now, skipping
     [ignored files](#ignored-files) (`*.md` by default);
   - `Cargo.toml.orig` against the local `Cargo.toml`;
   - if the member's own `Cargo.toml` is the same, what it **inherits from the workspace**:
     dependency versions and features (`serde.workspace = true` with a new `serde` version in the root
     `[workspace.dependencies]`), `[features]`, `edition`, `rust-version`, `license`.
3. **Bump level.** The commit the crate was published from is read from `.cargo_vcs_info.json`.
   Commits since then that touch the package directory are parsed as
   [Conventional Commits](https://www.conventionalcommits.org/); commits that only change ignored
   files don't count.

   | Current version | Breaking (`feat!:`, `BREAKING CHANGE:`) | `feat:`   | anything else |
   |-----------------|-----------------------------------------|-----------|---------------|
   | `0.x.y`         | minor                                   | patch     | patch         |
   | `>= 1.0.0`      | major                                   | minor     | patch         |
   | `x.y.z-pre.N`   | `pre.N+1`                               | `pre.N+1` | `pre.N+1`     |

   In a shallow clone (the default in most CI) the source commit is usually missing: the history
   is fetched once with `git fetch --unshallow` (disable with `--no-fetch`). If the commit is still
   unknown, the bump is a patch.
4. **Dependents.** Workspace members depending on a bumped crate get their version requirement
   updated and a patch bump, transitively. This covers `[dependencies]`, `[build-dependencies]`,
   `[target.*.dependencies]` and `[workspace.dependencies]`. A `[dev-dependencies]` requirement is
   updated only when it no longer matches the new version.
5. **Shared version.** Members with `version.workspace = true` are bumped together, to the highest
   version among their bumps; `[workspace.package] version` is updated.
6. **Write.** Manifests are edited in place, keeping formatting and comments, then
   `cargo update --workspace` refreshes `Cargo.lock`.

Registry lookups, downloads and comparisons run in parallel.

Example:

```text
$ cargo publish-plz update
git_cmd: 0.8.0 -> 0.9.0 (breaking: `src/lib.rs` changed, 1 commit)
release_plz_core: 0.38.7 -> 0.38.8 (dependency `git_cmd` updated)
  crates/release_plz_core/Cargo.toml: `git_cmd` 0.8.0 -> 0.9.0
```

### Explicit versions

`--bump` and `--version` skip change detection for the selected packages and bump them anyway;
dependents are still updated as usual.

```sh
cargo publish-plz update -p my-crate --bump minor     # 0.3.2 -> 0.4.0
cargo publish-plz update -p my-crate --version 1.0.0
```

`--bump` is applied literally (`major` on `0.3.2` gives `1.0.0`); a pre-release already at that level
is released as is (`patch` on `1.0.0-rc.1` gives `1.0.0`). The new version must be greater than the
current one.

### Options

| Option                   | Description                                                      |
|--------------------------|------------------------------------------------------------------|
| `--dry-run`              | Print what would change without writing anything                 |
| `--bump <LEVEL>`         | Bump the selected packages by `patch`, `minor` or `major`        |
| `--version <VERSION>`    | Set the selected packages to this version                        |
| `--ignore <GLOB>`        | Also ignore changes in matching files (repeatable)               |
| `--all`                  | Count changes in all files, ignoring nothing                     |
| `--no-fetch`             | Don't fetch git history in shallow clones                        |
| `--format <FORMAT>`      | `human` (default) or `json`                                      |
| `-p, --package <SPEC>`   | Only check these packages (dependents are still bumped)          |
| `--workspace`            | Check all workspace members                                      |
| `--manifest-path <PATH>` | Path to `Cargo.toml`                                             |
| `--registry <NAME>`      | Only check packages allowed to go to this registry               |

## `cargo publish-plz check`

Runs the same detection as `update` without writing anything, prints what `update` would do and
exits with code 1 if any package changed without a version bump. Packages already bumped and waiting
for `publish` are fine. Takes the same options as `update` except `--dry-run`, `--bump` and
`--version`.

```yaml
# .gitlab-ci.yml
release-check:
  script: cargo publish-plz check
```

## `cargo publish-plz publish`

- Packages that are already published at their current version are skipped with a warning, not an error,
  so the command can be safely re-run after a partial failure.
- The rest is published with `cargo publish -p a -p b ...`; Cargo orders the packages and waits for
  each one to appear in the index before publishing its dependents.
- Packages going to different registries are split into several `cargo publish` calls, ordered by
  their dependencies: with `base` (registry A) ← `mid` (registry B) ← `top` (registry A) the calls
  are `A: base`, `B: mid`, `A: top`. With `--dry-run`, calls depending on packages of an earlier call
  are skipped, since those packages were not uploaded.

| Option                   | Description                                          |
|--------------------------|------------------------------------------------------|
| `--dry-run`              | Perform all checks without uploading                 |
| `--allow-dirty`          | Allow uncommitted changes                            |
| `--no-verify`            | Don't build the packages before uploading            |
| `--format <FORMAT>`      | `human` (default) or `json`                          |
| `-p, --package <SPEC>`   | Publish only these packages                          |
| `--workspace`            | Publish all workspace members                        |
| `--manifest-path <PATH>` | Path to `Cargo.toml`                                 |
| `--registry <NAME>`      | Only publish packages allowed to go to this registry |
| `-- <ARGS>...`           | Extra arguments passed to `cargo publish`            |

## Ignored files

Changes in ignored files don't make a package changed, and commits touching only ignored files
don't count for the bump level. Patterns are globs relative to the package directory; `*` also
matches `/`, so `*.md` covers markdown files in every directory.

```toml
# Cargo.toml of the workspace: replaces the default ["*.md"]
[workspace.metadata.publish-plz]
ignore = ["*.md", "examples/**"]

# Cargo.toml of a package: replaces the workspace patterns for this package
[package.metadata.publish-plz]
ignore = ["*.md", "benches/**"]
```

`--ignore <GLOB>` adds patterns for one run; `--all` ignores nothing.

## Package selection

All commands pick packages the same way `cargo publish` does: `-p` and `--workspace` win; otherwise
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

Skipped packages are reported with a warning. Every `cargo publish` call gets an explicit
`--registry`. `update` and `check` use the same rules to choose the registry they compare against.

Registries other than crates.io are read from Cargo configuration:

```toml
# .cargo/config.toml
[registries.my-registry]
index = "sparse+https://my-registry.example.com/index/"
```

`CARGO_REGISTRIES_<NAME>_INDEX` overrides the config. If the index requires authentication, the token is
taken from `CARGO_REGISTRIES_<NAME>_TOKEN` or `$CARGO_HOME/credentials.toml`.

## JSON output

With `--format json` the result goes to stdout as JSON; progress and warnings stay on stderr.

`update` and `check`:

```json
{
  "packages": [
    {
      "name": "git_cmd",
      "version": "0.8.0",
      "status": "published",
      "registry": "crates-io",
      "inherited_changes": ["dependency `camino`"],
      "next_version": "0.8.1",
      "bump": "fix",
      "reason": "fix: inherited dependency `camino` changed, 0 commits"
    }
  ],
  "requirements": [
    { "manifest": "crates/release_plz/Cargo.toml", "dependency": "git_cmd", "old": "0.8.0", "new": "0.8.1" }
  ],
  "dry_run": true
}
```

- `status`: `new`, `pending` (bumped, not published yet), `published` or `not-allowed`;
- `changed_files` / `inherited_changes`: why a package changed;
- `next_version`, `bump` (`fix`, `feat`, `breaking`, `patch`/`minor`/`major`, `=VERSION`, `workspace`)
  and `reason` are present for packages that get a new version;
- `check` has `"ok": true|false` instead of `dry_run`.

`publish`: `packages[].outcome` is `published`, `dry-run`, `not-run`, `already-published` or
`not-allowed`, and `batches` lists the `cargo publish` calls.

## Limitations

- Only sparse registries are supported (crates.io or `sparse+...` indexes); git indexes are not.
- Credential providers are not supported for reading private indexes; use a token.

## License

[MIT](LICENSE)
