# plugin: git

English · [简体中文](README_CN.md)

Reads a **repository's history** into graph facts: commits, branches, tags,
merges, and the rebases only the reflog remembers. Manifest `git@1`, claims
**no file extension** — see [Dispatch](#dispatch). No `git` binary is
executed and none is required: the plugin reads `.git` itself, inside the
sandbox, through the host's two calls.

Every other plugin here reads *source* — the tree, as it is now. This one
reads the second source of truth sitting beside it: who changed what, in
what order, on which branch, and which of those commits were later thrown
away. None of that is visible to a walk of the working tree, and all of it
is exact — a commit's parents are not inferred, they are written down.

## Layout

```
parser/     drsg-git-parser — the object store, refs and reflog: a plain
            library, 15 native tests against repositories `git` builds
component/  drsg-plugin-git — the wasm wrapper: Guest impl + rmp-serde partial
```

## Dispatch

Routing everywhere else is by a file's extension, because that is what the
input is. A repository's history is not a file, so this plugin declares no
extensions and the file router never dispatches to it. The **host** runs it
instead, when the directory being digested turns out to have a git directory
in it — a fact the host can see rather than a guess — and hands it a view
rooted at that git directory:

```
HEAD  refs/heads/main  packed-refs  logs/HEAD  objects/pack/pack-….idx  …
```

That is a *tighter* grant than the working tree every code plugin gets. This
plugin cannot read a single source file, and the tree's plugins cannot read a
single object: `.git` is excluded from the ordinary walk, and always was.

The facts land in a plane of their own, `<plane>_git`, beside the code plane
the same digest writes. The two answer different questions and have different
lifetimes: a code plane is a picture of the tree *now* and is rewritten
whenever a file changes, while history only ever grows.

## What it produces

| Node | Key | What it is |
|---|---|---|
| `Commit` (also `Merge`) | `commit:<sha>` | one commit; `Merge` when it has two or more parents. `files` lists the paths it changed, `files_count` the same as an orderable integer |
| `Branch` (also `Remote`) | `branch:<refname>` | a branch, local or remote-tracking |
| `Tag` | `tag:<refname>` | a tag, annotated or lightweight |
| `Rebase` | `rebase:<ref>@<time>` | one replay, reconstructed from the reflog |

| Edge | Between | Says |
|---|---|---|
| `PARENT` (`order`) | commit → commit | the whole merge structure: `order = 1` is the line the commit was made on, the rest is what a merge brought in |
| `TIP` | branch → commit | where the branch points |
| `TAGS` | tag → commit | what the tag names, annotated tags peeled |
| `ONTO` | rebase → commit | what the replay was based on |
| `REPLACED` | rebase → commit | the tip it rewrote away |
| `PRODUCED` (`step`, `kind`) | rebase → commit | each commit it wrote |
| `RESULT` | rebase → commit | the tip the branch ended on |
| `ON` | rebase → branch | the branch it ran on |

A merge is a second label rather than a different first one, so
`MATCH (c:Commit)` still finds every commit and `MATCH (m:Merge)` finds the
joins. Ordering is by `committed_ts` / `authored_ts`, which are integers for
exactly that reason; `committed_at` and `authored_at` carry the same instants
as ISO-8601 in the committer's own timezone.

`files` is what joins a history to the code beside it — a path is the one
thing both planes name, and planes are hard namespaces, so the join is a read
of both rather than an edge. A path is recorded for a merge only when it
differs from **every** parent, which is git's own `TREESAME` rule: an ordinary
merge therefore lists nothing, because its side commits already account for
what it brought in, and what survives the rule is the conflict resolution.
Renames are two paths, not a rename — git stores a tree, and similarity
scoring is a porcelain display choice, not a fact in the object store.
`files_partial` appears, and only when true, where the walk had to stop short:
a parent outside the graph (a shallow clone), or a tree past the read, path or
depth limits. Without it a short list would answer "this commit left that file
alone" when it does not know.

## The one thing that cannot be exact

**A rebase leaves no trace in the commit graph at all.** It writes new
commits and moves a ref; nothing in any object records that one commit
replaced another. The only record is the **reflog** — local to a single
clone, and expiring (`gc.reflogExpire`, 90 days by default). So:

- a `Rebase` node means the reflog still remembers that replay;
- the *absence* of one means nothing at all, and the plugin says so in its
  report rather than letting silence read as "no rebase happened";
- a fresh clone has no reflog worth reading, so it has no rebases to show.

The same reflog is why commits no branch can reach are kept rather than
skipped: the commits a rebase abandoned are still in the object store, and a
graph showing the rewritten branch without what it replaced could not answer
"what did this rebase change?". Those commits carry `reachable: false`.

## Settings

`[plugins.git]` in the operator's `drsg.toml`:

| Setting | Default | What it does |
|---|---|---|
| `max_commits` | `20000` | Ceiling on commits, newest first; `0` reads everything. History has no natural size — a mature kernel tree is over a million commits — so a stated limit beats an hour nobody asked for. When it bites, the report says so. |
| `reflog` | `true` | Read the reflog: rebases, and the commits they left behind. |
| `remotes` | `true` | Include remote-tracking branches. |
| `tags` | `true` | Include tags. |
| `body` | `true` | Keep each commit message's body, not just its first line. |

An unknown key is an error naming the known ones: a misspelled `max_commit`
that silently kept the default would surface much later as a truncated graph
nobody asked for.

## How it reads `.git`

Only what history needs — **commits and annotated tags**. Trees and blobs are
never asked for, which is why this is a few hundred lines rather than a git
implementation: no working tree is reconstructed, no diff is taken, and the
size of a repository's *content* never enters into it.

- **Loose objects** are one zlib stream per file.
- **Packed objects** are found through the `.idx` (v2; a v1 index is refused
  rather than guessed at) and may be stored as deltas against another object
  by offset or by name. Both delta forms are resolved, recursively.
- **Refs** come from `refs/…` and `packed-refs`, loose winning where both
  exist — which is exactly how an update becomes visible before the next
  `git gc`.
- Commits are walked **from the tips**, newest first, so a repository with a
  hundred thousand loose objects costs what its *history* costs rather than
  what its content does.

A pack is read whole, because the host's `read` is whole-file — there is no
seek across the sandbox boundary. That is the one real cost of doing this
from inside the sandbox, and it is bounded by the largest pack rather than by
the repository's age.

Not handled, and said rather than guessed at: SHA-256 repositories, borrowed
objects (`objects/info/alternates`), and a `.git` *file* — a linked worktree
or a submodule, whose real git directory is outside what the host will answer
for. Each is reported by name.

## Build & test

```console
$ cd parser && cargo test          # 15 tests, against repositories git builds
$ just git-plugin                  # cargo build --release --target wasm32-wasip2
$ drsg plugin install plugins/git/component/target/wasm32-wasip2/release/drsg_plugin_git.wasm
$ drsg digest . --apply            # code → <plane>, history → <plane>_git
```

Nothing in `parser/`'s tests is a captured fixture. A hand-written `.git`
would only prove this code agrees with itself, and the whole risk in reading
an object store is disagreeing with git about a format git owns — so every
test builds a repository, does something to it (merge, rebase, tag,
`git gc --aggressive`), and reads what git actually wrote.
