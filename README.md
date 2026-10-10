# cargogc

Reclaim the cargo build caches that cargo will never read again — with an inode-honest
accounting of what deleting them actually returns, and a fail-closed gate that refuses to
delete anything while a build is running.

The judgement is **a build record, not an age**.

```console
$ cargogc plan --target-dir ./target --superseded-since 1791618600
cargogc 0.1.0 — plan — target: ./target
  since: 1791618600 (2026-10-10 15:50:00)   [--superseded-since]
  keep newest roots per crate: 1
  gate: free

  delete  superseded-root      debug/incremental/libfoo-3f9ac21…      1.09 GiB  (no session at or after T: this root did not take part in the build)
  delete  superseded-session   debug/incremental/libbar-71bb0e4…/s-…  0.24 MiB  (no build has touched this session since T)
  delete  superseded-lock      debug/incremental/libbar-71bb0e4…/s-2-1.lock  (no session left to guard)

  …
  freed (du, inode-honest)          8.55 GiB
  logical (per path)               10.66 GiB   inflation 1.25x
  shared, still allocated           1.30 GiB   (12153 inodes hardlinked outside the delete set)

  dry-run: nothing was deleted. Re-run as `cargogc apply` with the same arguments.
```

## What it reclaims, and on what grounds

Every class is a mechanical criterion plus a one-line cost. Nothing is decided by age
alone, and nothing is decided by "this looks unused".

| class | criterion | cost of being wrong |
|---|---|---|
| `dead` | the caller declares the tree unreadable by cargo (`--dead-dir`) | a rebuild |
| `superseded-root` | no session in that `<crate>-<hash>` root is at or after `T`, and it is not the newest root of its crate name | one recompile of that crate |
| `superseded-session` | a session older than `T` inside a root that *is* live | none worth naming |
| `superseded-lock` | an `s-<h1>-<h2>.lock` whose sessions are all gone | none; rustc recreates it |
| `capped` | a live incremental directory at or over `--live-incremental-max` (opt-in) | a full rebuild of that tree |

`T` is the start of the last build, which is why it is supplied by the caller
(`--superseded-since`, or `--last-build-file` written by a build hook) instead of being
guessed. Three consequences that are deliberate:

* **No record ⇒ no action.** Without `T` the superseded classes do nothing at all. An age
  gate ("same crate, several hashes, nothing touched for N days") reclaims nothing in a
  repository that is built all day — measured: 0 B — and guessing `T` would delete the
  sessions the next build is about to reuse.
* **A crate name keeps its newest root.** Two hashes of one crate name can be two live
  feature configurations, so the newest one is compacted to its newest session instead of
  being deleted. `--keep-newest-roots N` adjusts that (`0` disables it).
* **The gate is checked before anything is removed**, and never downgraded. A held lock —
  cargo's `target/<profile>/.cargo-lock` *or* rustc's own
  `incremental/<crate>-<hash>/s-<h1>-<h2>.lock`, which an editor's `cargo check` holds
  without cargo's — means exit 3 and zero deletions.

## The four numbers

Every run prints all four, because they answer three different questions and only one of
them is "how much do I get back":

| number | what it is | what it answers |
|---|---|---|
| **freed (du, inode-honest)** | `st_blocks × 512`, deduplicated by `(device, inode)`, and counted only when **every** link to that inode is inside the delete set | how much space actually comes back |
| **logical (per path)** | `st_size` summed over paths, no deduplication | why the tree *looked* so big |
| **shared, still allocated** | bytes whose inode is still referenced from outside the delete set | the part that does **not** come back |
| **inflation** | `logical / freed` | how misleading the logical number is |

The third number is not academic. On the tree this was written against, deleting 164
incremental entries changed the *subtree* `du` by 9.82 GiB while the enclosing target tree
— and an inode-level simulation — both said 8.55 GiB: the difference was `metadata.rmeta`
files hardlinked from `deps/`. The path went away; the blocks did not.

`freed` counts **directory inodes too**, and their block size is filesystem-dependent: a
deleted directory's own block is real space that comes back, so ext4 (4 KiB per directory)
reports slightly more than APFS (0). That is the more correct answer — but it means a test
that pins an exact total is pinning a filesystem, which is how the first version of the
hardlink test went red on Linux only. The suite now asserts on the blob it constructed.

## Usage

```console
# A build hook, after a successful build: record when the build started…
$ date +%s > .local/last-build-start.epoch

# …then let every entry point (the hook, a cron job, a human) use the same record:
$ cargogc plan  --target-dir ./target --last-build-file .local/last-build-start.epoch
$ cargogc apply --target-dir ./target --last-build-file .local/last-build-start.epoch \
      --ledger .local/cargogc-runs.jsonl

# A one-off, with the timestamp spelled out (unix seconds, or ISO-8601 with an offset):
$ cargogc apply --target-dir ./target --superseded-since 2026-10-10T15:50:00+08:00

# Cap the live incremental tree instead, when there is no useful build record:
$ cargogc apply --target-dir ./target --live-incremental-max 8GiB

# What did past runs do, and is anything left?
$ cargogc ledger --view --ledger .local/cargogc-runs.jsonl
$ cargogc plan --target-dir ./target --last-build-file … --reconcile \
      --ledger .local/cargogc-runs.jsonl
```

`--target-dir` defaults to the `target/` directory found by walking up from the current
directory.

`cargogc` does not read `Cargo.toml`. Which profile trees cargo can never read is the
caller's knowledge — different repositories have different profiles and feature
combinations — and it arrives as `--dead-dir` (repeatable). Everything else is judged from
the tree itself.

### Options

| option | meaning |
|---|---|
| `--target-dir DIR` | the cargo target directory; default: search upwards for `target/` |
| `--scan-root DIR` | batch mode (repeatable): sweep every cargo target directory at or below `DIR`. Mutually exclusive with `--target-dir`; `--dead-dir` does not compose with it (see below) |
| `--superseded-since T` | the build record: unix seconds, or ISO-8601 **with an offset** |
| `--last-build-file PATH` | read the build record from a file (unix seconds). Missing or empty ⇒ no record ⇒ the superseded classes do nothing |
| `--live-incremental-max SIZE` | opt-in cap; `KiB/MiB/GiB` are 1024-based, `KB/MB/GB` are 1000-based, and a bare number is refused |
| `--keep-newest-roots N` | protect the newest `N` roots per crate name (default `1`) |
| `--dead-dir DIR` | a tree cargo can never read (repeatable) |
| `--ledger PATH` | append `apply`/`skipped` events as JSONL |
| `--json` | machine-readable report |
| `--reconcile` | also compare against the ledger's last apply |
| `-h, --help`, `-V, --version` | |

### Exit codes

| code | meaning |
|---|---|
| `0` | ok — including "nothing to reclaim" and a clean `plan` |
| `1` | runtime error (unreadable tree, an unusable build-record *file*, a delete that failed) |
| `2` | usage error (unknown option, bare size, relative timestamp, no target directory) |
| `3` | **refused**: a build holds a lock, or the gate cannot probe — nothing was deleted |

Relative timestamps such as `2h ago` are refused, and so are local ISO-8601 times without
an offset. Both mean something different every time they are read, while the whole
judgement rests on "which builds happened after `T`". Add an offset, or pass the unix
seconds that a build hook already has.

## Batch mode: `--scan-root`

```console
# One command, every repository under a tree. The nightly shape:
$ cargogc apply --scan-root ~/src --superseded-since "$(date -v-1d +%s)" \
      --ledger ~/.local/cargogc-runs.jsonl
```

Running per repository is how repositories get missed — the ones nobody remembers are
exactly the ones nothing sweeps. `--scan-root` (repeatable) walks each root and sweeps
every cargo target directory it finds.

**Discovery.** Up to **4 levels** below each root; `.git`, `.jj` and `node_modules` are
never descended into, and neither is a directory once it is recognised as a target
directory (its `deps/` and `incremental/` are cargo's bookkeeping, not nested
repositories). A directory is a target directory when one of its children is a *profile
tree*:

* `debug` or `release`, with a `deps/` or an `incremental/` inside — the common case;
* any other name (`aarch64-apple-darwin`, or a custom profile such as `profiling`) with
  `deps/` **and** either `incremental/` or `.cargo-lock`.

Requiring two cargo-specific markers for the unnamed case is what keeps an ordinary
source directory named `…/incremental` or `…/deps` from being mistaken for a profile tree,
while still catching custom-profile layouts that a `debug`/`release`/triple-only rule would
skip silently — and a silently skipped repository is the failure this mode exists to
prevent.

**Four differences from single-target mode.** They are also in `--help`, and the report
shows them, because a batch round that behaves differently without saying so is a trap:

1. **A held lock skips that target and the round continues.** Single-target mode refuses
   the whole run with exit 3; a nightly batch over twenty repositories must not be voided
   by one of them happening to build. The skipped target is named in the report *and* in
   the ledger — never silently dropped — and the round still exits **0**.
2. **`dead` does not participate.** That class comes from a repository's own manifest, and
   a scan has no manifest per target, so the tool says `dead: skipped (scan mode has no
   manifest)` instead of guessing. For the same reason `--dead-dir` is refused together
   with `--scan-root` (exit 2).
3. **One build record for the whole round.** `--superseded-since` / `--last-build-file`
   are a policy you set once, and the natural nightly policy is "nothing has touched it for
   24 hours": `--superseded-since "$(date -v-1d +%s)"`. Relative forms are still refused —
   the timestamp is resolved once, before the round, not once per target.
4. **`--keep-newest-roots` matters more, not less.** With one policy across many
   repositories it is the guarantee that every crate name in every repository keeps at
   least one root. The default is already `1`.

`plan` never skips: it deletes nothing, so a lock is reported per target (`gate: a build
holds …`) and the round still shows what is waiting there. But a **gated target contributes
nothing to the round total** — in a preview as well as in an apply — so `plan` and `apply`
agree on what comes back while the same target is held (its row is marked `*`, like a skipped
one). Doing it the other way round is how a preview promised "3 items" where apply deleted 2.

**Output.** The human report is one line per target — path, items, freed `du`, gate — then
the round totals and the list of skipped targets, with per-item lines left out on purpose
(a batch over twenty repositories would print thousands):

```console
$ cargogc apply --scan-root ~/src --superseded-since 1791618600
cargogc 0.1.3 — apply — scan mode: 3 target(s) under 1 root(s)
  roots: /home/me/src
  dead: skipped (scan mode has no manifest)
  since: 1791618600 (2026-10-10 07:50:00)   [--superseded-since]
  keep newest roots per crate: 1

  apply  /home/me/src/api/target        12 items      1.09 GiB freed   gate free
  skip   /home/me/src/web/target         4 items    180.0 KiB freed   gate a build holds /home/me/src/web/target/debug/.cargo-lock
  apply  /home/me/src/tools/target       0 items           0 B freed   gate free

  target(s): 3 (2 applied, 1 skipped)
  freed (du, inode-honest)       1.09 GiB
  ...
  skipped targets (nothing was deleted in these):
    /home/me/src/web/target   a build holds /home/me/src/web/target/debug/.cargo-lock
```

The numbers on a skipped row are what that target's plan *would* have reclaimed — useful
when you want to know what the round missed — and they are excluded from the round totals.

`--json` returns `targets[]` (each with `target`, `verdict`, `gate`, `classes`, `totals`,
and a `skipped` object when it was skipped) plus `scan_totals` for the round, and a
top-level `dead` saying the class did not participate.

**Ledger.** Scan mode appends **one run** for the round, whose `finish` record carries
`targets[]` — the single-target schema is untouched. `ledger --view` prints both shapes in
one table (a scan row shows `N targets (M skipped)`):

```console
$ cargogc ledger --view --ledger ~/.local/cargogc-runs.jsonl
cargogc 0.1.3 — ledger — /home/me/.local/cargogc-runs.jsonl
  2026-10-10T07:35:10Z apply   applied        1 items     40.0 KiB freed   target /home/me/src/api/target
  2026-10-10T07:50:02Z apply   applied      106 items      8.55 GiB freed   3 targets (1 skipped)
```

A scan run's totals count only the targets it actually swept; the skipped ones are listed
in the record, so "the round freed 8.55 GiB" can never quietly include bytes that are
still on disk.

## Platform boundary: `flock`

`apply` needs to prove that no build is writing into the tree, and it does that by taking
a non-blocking exclusive `flock` on every candidate lock. **Windows has no `flock`**, so
the question cannot be answered there and the fail-closed answer is the only one this tool
gives: `apply` exits 3 and deletes nothing. `plan` works everywhere (reading a tree needs
no lock), and so does `--json`.

This is why CI runs the full suite — including the negative controls that hold a real lock
and assert that a refusal removes zero bytes — on Linux and macOS, and only builds on
Windows.

## Why not `cargo-sweep`, and why not a release-size tool

* **`cargo-sweep`** handles *expired* artifacts, which it does well, and this tool does not
  reimplement that. It does not touch `incremental/` at all (measured: it empties `deps/`
  to zero files and leaves `incremental/` untouched), and `incremental` is the class that
  actually grows. It also reports logical bytes, which overstates what deleting returns.
* **A release-artifact size tool** answers a different question with a different lifetime:
  it budgets the bytes you *ship* and measures them per release, while this is about the
  bytes a *development* tree keeps. Merging the two would produce one program with two
  unrelated jobs and two unrelated notification stories.
* **Waiting for rustc to garbage-collect its own sessions** is the right long-term fix and
  is worth tracking, but until it exists every repository has to sweep for itself.

What this tool deliberately does *not* do: touch `deps/` (that is the working set that
keeps a no-op build at ~0.1 s, and its `.o` files are hardlinked), run as a daemon, hold a
timer, or guess a `T` when the caller has no record.

## Tests

```console
$ cargo test
```

The suite is fixture-based and runs the real binary. The negative controls matter most: a
held lock (session lock, and separately cargo's lock) must produce exit 3 **and leave a
byte-for-byte identical tree**, which the tests check by snapshotting the whole fixture
and comparing. Batch mode repeats that control per repository: a locked repository must be
skipped, named, and come out byte-for-byte unchanged while the rest of the round proceeds. The accounting tests build a hardlink that leaves the delete set and assert
that its bytes are reported as *still allocated* rather than freed.

## License

MIT.
