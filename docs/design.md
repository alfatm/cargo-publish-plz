# Design

How `cargo publish-plz` decides what to bump, and the edge cases it handles. The [README](../README.md)
has the overview; this file has the reasons.

The registry is the only source of truth. There are no tags, release commits or state files to keep in sync:
what was released is whatever the registry has, and where it came from is the `.cargo_vcs_info.json` cargo
packs into every `.crate`. Packages that are never published have git instead: the commit that set the version
([Unpublished packages](#unpublished-packages)).

## Registry status

Every publishable package (`publish != false`) is first looked up in the index of its registry
([Registries](#registries)). The outcome decides what happens next:

| Status               | When                                                                  | Then                |
|----------------------|-----------------------------------------------------------------------|---------------------|
| `new`                | never published                                                       | published as is     |
| `pending`            | the local version is not in the registry                              | waits for `publish` |
| `published`          | the local version is in the registry                                  | compared            |
| `behind`             | below the newest release, made from this history or after it          | left alone          |
| `version-taken`      | changed, but the next version was released from another history       | not bumped, fails   |
| `not-allowed`        | `package.publish` doesn't allow the `--registry` one                  | skipped             |
| `ambiguous-registry` | `package.publish` lists several registries, no `--registry` picks one | skipped, fails      |

Yanked versions count as published (they can't be published again) but never as the newest release.

Packages with `publish = false` are looked up in git instead ([Unpublished packages](#unpublished-packages)),
and ones with `update = false` in neither:

| Status        | When                                                      | Then       |
|---------------|-----------------------------------------------------------|------------|
| `new`         | `publish = false`, `HEAD` has no such package             | left alone |
| `pending`     | `publish = false`, `HEAD` has another version             | left alone |
| `committed`   | `publish = false`, `HEAD` has the local version           | compared   |
| `disabled`    | `update = false`                                          | warned     |
| `unversioned` | `publish = false` without `version` (cargo takes `0.0.0`) | left alone |

### Behind: the version is not kept in the repository

Some repositories don't commit version bumps: CI sets the version at publish time. Their `Cargo.toml` says
`0.54.0` forever while the registry has `0.58.0`. Comparing with `0.54.0` would report every change since then
as unreleased, so such a package is `behind` and not compared.

The same holds for an old checkout: a clone not pulled since `0.58.0` was released from a later commit.

The rule: the newest release (yanked ones aside) is above the local version, and its commit is in the history
of `HEAD` or `HEAD` is in its history. The commit comes from the release's `.cargo_vcs_info.json`.

- **Another line is not behind.** A newer release made from a branch that never reached this history (a
  backport line, a pre-release branch) says nothing about this one: the package is compared as usual.
- **Pre-releases don't supersede a stable line.** For a stable local version only stable releases count as
  newer, so `1.0.x` is still compared after a `1.1.0-beta.1` was published from main.
- **Unknown counts as behind.** No `.cargo_vcs_info.json` (packaged outside git), not a git repository, or a
  shallow clone that can't be deepened (`--no-fetch`): comparing with an older release is what reports changes
  that are not there, so the safe answer is to leave the package alone.
- **Packages that weren't asked for.** With `-p`, the other members only matter for propagation. When the
  newest `.crate` of one of them can't be downloaded (a 401 or 500 from the registry), that is a warning and
  the package counts as `behind`, instead of failing the run for the packages that were asked for.

Each newer release is downloaded only once: the commit read from its `.cargo_vcs_info.json` is cached by the
`.crate` checksum, which identifies a published file that never changes. Only the first archive entry is read;
cargo packs `.cargo_vcs_info.json` first. The cache is in `$XDG_CACHE_HOME/publish-plz`
(`%LOCALAPPDATA%` on Windows, else `~/.cache`). An empty or relative variable is ignored, as the XDG spec asks:
CI images set `XDG_CACHE_HOME=`, and a relative cache would be written into the repository being checked and
could end up in a commit or a package.

### Version taken: the next version came out of another history

A package compared with an older release of its own can come out with a next version the registry already
has. `0.30.4` changed in a breaking way asks for `0.31.0`, while `0.31.0`…`0.34.0` were published from commits
this history doesn't have: another branch, or a remote that wasn't fetched. `update` would write a taken
version and `publish` would fail on it.

Such a package is `version-taken`:

- it is not bumped, and nothing is bumped because of it: propagation starts over without it;
- `check` and `update` exit with 1, because the change is still unreleased; `update` writes the other bumps
  all the same;
- a dependent that would only have moved because of it doesn't move, and isn't `version-taken` itself;
- members sharing `workspace.package.version` move together, so when one of them can't take the group's next
  version, none of the published ones does (never-published and pending members keep their status);
- versions asked for with `--bump` / `--version` are not checked: the developer chose them, a collision is
  left to `publish`.

The way out is to fetch the history the releases came from, or to set a version past the newest release.

### Below the newest release: backport or stale branch

A branch that left before the newest release still gets a free next version: `1.0.0` with a fix asks for
`1.0.1` while `1.1.0` is out. That is exactly what a backport line wants, and a mistake for a feature branch
that is to be merged. The history looks the same in both cases (neither release commit is an ancestor of the
other), so nothing can tell them apart. Refusing would make every maintenance branch pass a flag forever; so
it is a warning instead, and the `reason` says `below the newest published 1.1.0`. Merge or rebase onto the
release first unless it is a backport.

## Change detection

For a `published` package the `.crate` of the local version is downloaded and compared with the local package:

- file by file with what `cargo package --list` would pack now, skipping [ignored files](../README.md#ignored-files);
  the files cargo generates (`Cargo.toml`, `Cargo.lock`, `.cargo_vcs_info.json`) are left out;
- `Cargo.toml.orig` against the local `Cargo.toml`;
- if the member's own `Cargo.toml` is the same, what it **inherits from the workspace**: dependency versions
  and features (`serde.workspace = true` with a new `serde` in the root `[workspace.dependencies]`),
  `[features]`, `edition`, `rust-version`, `license`.

`readme` and `license-file` outside the package directory are packed at the package root and compared there.

## Bump level

Commits between the release's commit and `HEAD` that touch the package directory are parsed as Conventional
Commits; commits that only change ignored files don't count, and neither do commits to nested members (the
root package of a workspace doesn't take its members' commits). Files that differ without any commit
(uncommitted changes, a workspace-level change) count as a fix. When the release's commit is unknown even
after fetching, the bump is a patch, with a warning.

`--bump` is applied literally (`major` on `0.3.2` gives `1.0.0`); a pre-release already at that level is
released as is (`patch` on `1.0.0-rc.1` gives `1.0.0`). The new version must be greater than the current one.

## Propagation

Members depending on a bumped crate get their version requirement updated and a patch bump, transitively.
This covers `[dependencies]`, `[build-dependencies]`, `[target.*.dependencies]` and
`[workspace.dependencies]`. A `[dev-dependencies]` requirement is updated only when it no longer matches the
new version, since dev-dependencies don't reach users. Only `published` and `committed` members are bumped by
propagation.

Members with `version.workspace = true` are bumped together, to the highest version among their bumps, and
`[workspace.package] version` is updated.

## Unpublished packages

A `publish = false` package can be a git dependency of another repository, and cargo checks the `version` of a
git dependency against the requirement, so a breaking change under the same version breaks its users quietly.
It is bumped like a published one, with git as the record of releases: a version is released by the commit that
set it.

That commit is the newest one touching the package's `Cargo.toml` (and the workspace one, when the package takes
`version.workspace = true`) whose first parent has another version of the package, or none (the package was
added, renamed or moved there). In history, a manifest without `version` has cargo's `0.0.0`. The version is compared at
`HEAD` first: a package `HEAD` doesn't have is `new`, and one whose version differs from `HEAD`'s is `pending`
(the bump is not committed yet), so running `update` twice doesn't bump twice.

A `committed` package changed when its files differ from the release commit: `git diff` against it plus
untracked files, in the package directory without nested members, [ignored files](../README.md#ignored-files)
left out. What it inherits from the workspace is not compared, short of the version requirements on bumped
members, which propagation covers. The bump level comes from the commits since, as for a published package,
and propagation goes through `committed` packages as through `published` ones.

A selected package whose release commit can't be found is an error, never a guess: outside a git repository,
or beyond a shallow clone's history that `--no-fetch` keeps from being fetched. A package that is not
selected and only there for propagation is taken as released outside a git repository.

### `update = false`

`[package.metadata.publish-plz] update = false` (default `[workspace.metadata.publish-plz] update`, else
`true`) leaves the version alone whatever changed, published or not, `--bump` and `--version` included. The
package is not compared, propagation doesn't go through it, and `update` / `check` warn instead: about every
selected one, and about any other whose requirement on a bumped member is rewritten. The requirement is still
rewritten, or the workspace wouldn't build. A package sharing the workspace version moves with it all the same,
with a warning: the version is one field.

## Shallow clones

CI usually clones with `--depth`. The commits releases were made from are then missing, or present (as the tip
of another branch fetched with `--no-single-branch`) but cut off from `HEAD` by the shallow boundary, so
`git merge-base --is-ancestor` says no although the answer is yes. Whenever a relation to `HEAD` isn't found in
a shallow clone, or the commit that set an [unpublished package's](#unpublished-packages) version is beyond
it, the full history is fetched once with `git fetch --unshallow`, and only the commits not found
are asked again. `--no-fetch` disables this; relations a shallow clone can't show then count as unknown.

`is_shallow` runs once per run, the ancestry of each commit once (again only after a real fetch), and the
lookups run in parallel, like the registry requests and downloads.

## Checking a revision

`check --rev <REV>` checks the committed state of a revision, so uncommitted and untracked files don't count
and the working tree is not touched. The revision is checked out into a temporary `git worktree` rather than a
clone: it shares the repository's objects, refs and shallow boundary, so `--rev origin/main` resolves without
copying anything, and `git fetch --unshallow` deepens the repository itself. `.cargo/config.toml` files are
looked up from the current directory, as cargo does, so a registry configured outside the repository is
still found.

### Where the worktree goes

The worktree is a hidden directory beside the repository, `.<repo>-publish-plz-XXXXXX`. Beside, because paths
that leave the repository (`path = "../other-repo/crate"`, a `[patch]` to a neighbouring clone) then resolve as
they do in the repository itself; from the system temporary directory they wouldn't.

It goes to the system temporary directory instead when:

- the parent directory can't be written;
- the repository is a submodule (of a project repository): beside it the worktree would show as untracked in
  the superproject, and a cargo workspace there would claim it. A submodule meant to be built on its own
  doesn't use paths leaving it anyway.

A clone that merely lies inside another repository's directory (`~/.git` for dotfiles) is not a submodule and
still gets its worktree beside it.

### Submodules

Submodules the revision pins are checked out as worktrees of the submodule clones in the repository, at the
pinned commit, recursively. Nothing is fetched, and the commit is exactly the pinned one. A submodule that isn't
checked out in the repository, or lacks the pinned commit, is left empty with a warning. On removal the nested
worktrees go first: git refuses to remove a worktree that still holds others.

### No hooks

The repository's hooks don't run, neither for the worktrees nor for `fetch --unshallow`: git is pointed at an
empty hooks directory (`-c core.hooksPath=…`) for those commands. A check only reads, while `post-checkout`,
`reference-transaction` and the like run whatever the repository installed (husky, lefthook, code
generation), slow every run down and can change files. Filters such as Git LFS still apply, so the files
compared are the real ones.

### Temporary directories

The worktree and the hooks directory are new directories with a random name that only the current user can
open (`0700`), and never ones that existed before. A predictable path (`/tmp/publish-plz-<pid>`) would let
another user of a shared machine or CI runner create it first, put a hook in it, and have it run with our
rights; the sticky bit on `/tmp` would even keep us from deleting it.

### Clean-up

The worktrees are removed when the check ends, and their records in `.git/worktrees` with them. Destructors
don't run on a signal, so every worktree and hooks directory is registered right after it is created, and a
Ctrl-C or SIGTERM (a cancelled CI job) removes them, nested worktrees first, before exiting with 130. A second
Ctrl-C doesn't reach a handler that is still running, so the clean-up gives up after 10 seconds should git
hang (on a lock, say). Only after `kill -9`, or such a hang, is something left: remove the
`.<repo>-publish-plz-*` directory and run `git worktree prune`.

## Registries

The registry of a package is the one `cargo publish` would pick: `--registry`, else the single entry of
`package.publish`, else `registry.default` (`CARGO_REGISTRY_DEFAULT`), else crates.io. A package listing
several registries needs `--registry`: `update` and `publish` stop with an error when it was selected, `check`
reports it as `ambiguous-registry`, checks the others and fails. An ambiguous package that wasn't selected only
matters for propagation and is left alone.

Configuration is read like cargo reads it: `CARGO_REGISTRIES_<NAME>_INDEX` / `_TOKEN`, then `.cargo/config.toml`
(and `.cargo/config`) from the current directory up to the root, then `$CARGO_HOME`. Tokens also come from
`$CARGO_HOME/credentials.toml`. A token is sent only when the registry asks for it: `config.json` answers 401,
or says `auth-required`.

All registries are connected up front so lookups and downloads can run in parallel.

## Publishing

Packages already published at their current version are skipped with a warning, not an error, so `publish`
can be re-run after a partial failure. The rest is published with one multi-package `cargo publish` per
registry: Cargo orders the packages and waits for each to appear in the index before publishing its
dependents.

Packages going to different registries are split into several calls ordered by their dependencies: with `base`
(registry A) ← `mid` (registry B) ← `top` (registry A) the calls are `A: base`, `B: mid`, `A: top`. A cycle
within one registry is left to cargo; one across registries is an error. Every call gets an explicit
`--registry`. With `--dry-run`, calls depending on packages of an earlier call are skipped, since those
packages were not uploaded.

## JSON output

`update` and `check`, per package in `packages`:

- `name`, `version`, `status` (see [Registry status](#registry-status)), `registry`;
- `registries`: for `ambiguous-registry`, the registries `package.publish` allows (selected packages only);
- `latest`: the newest published version that is not yanked;
- `published_sha`: for a `published` package, the commit its version was packaged from; for a `committed` one,
  the commit that set its version;
- `changed_files` / `inherited_changes`: why a package changed;
- `commits`: the commits since `published_sha` that touch the package and count for the bump, newest first;
- `next_version`, `bump` (`fix`, `feat`, `breaking`, `patch`/`minor`/`major`, `=VERSION`, `workspace`) and
  `reason`: for packages that get a new version. The `reason` of one below the newest release ends with
  `below the newest published X`; a `version-taken` package has a `reason` too
  (`the next version 0.31.0 is already published`).

`requirements` lists the requirement edits, `dry_run` says whether `update` wrote anything, and `check` has
`"ok": true|false` instead.

`publish`: `packages[].outcome` is `published`, `dry-run`, `not-run`, `already-published`, `not-allowed` or
`not-publishable` (`publish = false`),
and `batches` lists the `cargo publish` calls.
