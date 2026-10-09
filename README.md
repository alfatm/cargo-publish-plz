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

This installs two equivalent commands: `cargo publish-plz` (a Cargo subcommand) and the shorter
`publish-plz`. Examples below use the former; `publish-plz update` works the same way.

`publish` relies on multi-package `cargo publish`, available since Cargo 1.90.

## Typical flow

```sh
cargo publish-plz update      # bump versions, review the diff
git commit -am "chore: release"
cargo publish-plz publish     # publish everything that isn't published yet
```

In CI, `cargo publish-plz check` on merge requests catches a forgotten `update`.

## How it works

The registry is the source of truth: there are no tags or state files. For every publishable package:

1. **Compare with the release.** The published `.crate` of the local version is downloaded and compared
   with what `cargo package` would pack now, file by file, plus what the manifest inherits from the
   workspace (dependency versions, features, edition, …). Equal means unchanged.
2. **Pick the bump.** The commit the release was packaged from is in its `.cargo_vcs_info.json`. The
   commits since then that touch the package are read as
   [Conventional Commits](https://www.conventionalcommits.org/):

   | Current version | Breaking (`feat!:`, `BREAKING CHANGE:`) | `feat:`   | anything else |
   |-----------------|-----------------------------------------|-----------|---------------|
   | `0.x.y`         | minor                                   | patch     | patch         |
   | `>= 1.0.0`      | major                                   | minor     | patch         |
   | `x.y.z-pre.N`   | `pre.N+1`                               | `pre.N+1` | `pre.N+1`     |

3. **Propagate.** Members depending on a bumped crate get their requirement updated and a patch bump,
   transitively. Members sharing `workspace.package.version` move together.
4. **Write.** Manifests are edited in place, keeping formatting and comments, then
   `cargo update --workspace` refreshes `Cargo.lock`.

Packages with `publish = false` are bumped the same way, but their release is the commit that set the
local version: see [Unpublished packages](#unpublished-packages).

A package whose local version is below the newest release made from this history is `behind` and left
alone: the repository doesn't keep the version, or the checkout is old. One whose next version was already
released from another history is `version-taken` and fails `check`. These and the other edge cases
(backports, pre-releases, shallow clones, …) are explained in [docs/design.md](docs/design.md).

```text
$ cargo publish-plz update
git_cmd: 0.8.0 -> 0.9.0 (breaking: `src/lib.rs` changed, 1 commit)
release_plz_core: 0.38.7 -> 0.38.8 (dependency `git_cmd` updated)
  crates/release_plz_core/Cargo.toml: `git_cmd` 0.8.0 -> 0.9.0
```

## `cargo publish-plz update`

`--bump` and `--version` skip change detection for the selected packages and bump them anyway;
dependents are still updated as usual.

```sh
cargo publish-plz update -p my-crate --bump minor     # 0.3.2 -> 0.4.0
cargo publish-plz update -p my-crate --version 1.0.0
```

| Option                   | Description                                               |
|--------------------------|-----------------------------------------------------------|
| `--dry-run`              | Print what would change without writing anything          |
| `--bump <LEVEL>`         | Bump the selected packages by `patch`, `minor` or `major` |
| `--version <VERSION>`    | Set the selected packages to this version                 |
| `--ignore <GLOB>`        | Also ignore changes in matching files (repeatable)        |
| `--all`                  | Count changes in all files, ignoring nothing              |
| `--no-fetch`             | Don't fetch git history in shallow clones                 |
| `--format <FORMAT>`      | `human` (default) or `json`                               |
| `-p, --package <SPEC>`   | Only check these packages (dependents are still bumped)   |
| `--workspace`            | Check all workspace members                               |
| `--manifest-path <PATH>` | Path to `Cargo.toml`                                      |
| `--registry <NAME>`      | Only check packages allowed to go to this registry        |

## `cargo publish-plz check`

The same detection as `update`, without writing anything. Exits with 1 if a package changed without a
version bump, its next version is already taken, or it can go to several registries and no `--registry`
picks one. Packages already bumped and waiting for `publish` are fine. Takes the options of `update`
except `--dry-run`, `--bump` and `--version`.

`--rev <REV>` checks the committed state of a revision instead of the working tree. It is checked out
into a temporary `git worktree`, with its submodules and without running the repository's hooks, and
removed afterwards; see [docs/design.md](docs/design.md#checking-a-revision).

```sh
cargo publish-plz check --workspace --rev HEAD
cargo publish-plz check -p my-crate --rev origin/main
```

```yaml
# .gitlab-ci.yml
release-check:
  script: cargo publish-plz check
```

## `cargo publish-plz publish`

Publishes every package whose current version is not in its registry with `cargo publish -p a -p b ...`;
Cargo orders the packages and waits for each one to appear in the index before its dependents. Packages
already published are skipped, so the command can be re-run after a partial failure. Packages going to
different registries are split into calls ordered by their dependencies.

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

All commands pick packages the way `cargo publish` does: `-p` and `--workspace` win; otherwise running
inside a member directory selects that member, and running at the workspace root selects all members.
`publish` skips packages with `publish = false`, with a warning.

## Unpublished packages

A package with `publish = false` never reaches a registry, but it can still be used as a git dependency, and
cargo checks the `version` requirement of a git dependency against it. Without `publish = false` a package is
publishable, as cargo has it: one never published waits for its first `publish`, which releases the local version
as is, so it is not bumped however much it changes. A package that is only used through git needs
`publish = false` to be bumped. `update` and `check` bump it like any
other, with git in place of the registry: its release is the commit that set its current version (the newest
commit touching its `Cargo.toml`, or the workspace one for `version.workspace = true`, after which the
version is the current one). Files that differ from that commit, committed or not, make it changed; the
commits since pick the bump; it takes part in propagation. When that commit can't be found (no git
repository, a shallow clone that can't be deepened) a selected package is an error.

`update = false` leaves a package's version alone, published or not; `update` and `check` warn about it
instead, saying which dependency was updated under it, if any. It is `true` by default:

```toml
# Cargo.toml of the workspace: the default for every package
[workspace.metadata.publish-plz]
update = false

# Cargo.toml of a package: overrides the workspace
[package.metadata.publish-plz]
update = true
```

A package with `update = false` that shares the workspace version still moves with it, with a warning. A
`publish = false` package without `version` (cargo takes `0.0.0`) is `unversioned` and never bumped.

## Registries

Each package goes to the registry `cargo publish` would pick for it, honouring `package.publish`
(including `publish.workspace = true`):

| `package.publish` | without `--registry`                                     | with `--registry X`           |
|-------------------|----------------------------------------------------------|-------------------------------|
| `false`           | skipped                                                  | skipped                       |
| unset / `true`    | `registry.default`, else crates.io                       | `X`                           |
| `["Y"]`           | `Y`                                                      | `X` if `X == Y`, else skipped |
| `["Y", "Z"]`      | error: pass `--registry` (`check`: `ambiguous-registry`) | `X` if listed, else skipped   |

`update` and `check` compare against the same registry. Registries other than crates.io are read from
Cargo configuration, looked up from the current directory as cargo does:

```toml
# .cargo/config.toml
[registries.my-registry]
index = "sparse+https://my-registry.example.com/index/"
```

`CARGO_REGISTRIES_<NAME>_INDEX` overrides the config. If the index requires authentication, the token is
taken from `CARGO_REGISTRIES_<NAME>_TOKEN` or `$CARGO_HOME/credentials.toml`.

## JSON output

With `--format json` the result goes to stdout; progress and warnings stay on stderr.

```json
{
  "packages": [
    {
      "name": "git_cmd",
      "version": "0.8.0",
      "status": "published",
      "registry": "crates-io",
      "latest": "0.8.0",
      "published_sha": "3c1f0e2d9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d",
      "changed_files": ["src/lib.rs"],
      "commits": [{ "sha": "9e8f7a6b5c4d3c1f0e2d9a8b7c6d5e4f3a2b1c0d", "subject": "fix: retry on EINTR" }],
      "next_version": "0.8.1",
      "bump": "fix",
      "reason": "fix: `src/lib.rs` changed, 1 commit"
    }
  ],
  "requirements": [
    { "manifest": "crates/release_plz/Cargo.toml", "dependency": "git_cmd", "old": "0.8.0", "new": "0.8.1" }
  ],
  "dry_run": true
}
```

`status` is one of `new`, `pending`, `published`, `committed`, `disabled`, `unversioned`, `behind`,
`version-taken`, `not-allowed` and `ambiguous-registry`; `check` has `"ok": true|false` instead of `dry_run`. Every field is described in
[docs/design.md](docs/design.md#json-output).

## Limitations

- Only sparse registries are supported (crates.io or `sparse+...` indexes); git indexes are not.
- Credential providers are not supported for reading private indexes; use a token.

## License

[MIT](LICENSE)
