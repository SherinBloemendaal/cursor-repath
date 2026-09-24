<p align="center">
  <img src="assets/banner.png" alt="Cursor Repath" width="100%">
</p>

## Install

### macOS / Linux

```bash
curl -fsSL https://sherin.dev/crepath/install.sh | bash
```

Same script from GitHub, if sherin.dev is unavailable:

```bash
curl -fsSL https://raw.githubusercontent.com/SherinBloemendaal/cursor-repath/main/install.sh | bash
```

Pin a release with `bash -s`:

```bash
curl -fsSL https://sherin.dev/crepath/install.sh | bash -s v1.0.0
```

The script installs `crepath` to `~/.crepath/bin` (override with `CREPATH_INSTALL`). It adds that directory to your zsh, bash, or fish config when the line is missing. Run it again to upgrade in place.

### Windows

```powershell
powershell -c "irm https://raw.githubusercontent.com/SherinBloemendaal/cursor-repath/main/install.ps1|iex"
```

`$env:CREPATH_VERSION` pins a tag (`v1.0.0`). `$env:CREPATH_INSTALL` overrides the install directory (default `%USERPROFILE%\.crepath\bin`). The script adds that directory to the user PATH when it is missing.

Published archives, checked against `SHA256SUMS` on the GitHub release:

| Platform             | Asset                                      |
| -------------------- | ------------------------------------------ |
| macOS Apple Silicon  | `crepath-aarch64-apple-darwin.tar.gz`      |
| macOS Intel          | `crepath-x86_64-apple-darwin.tar.gz`       |
| Linux x86_64 (glibc) | `crepath-x86_64-unknown-linux-gnu.tar.gz`  |
| Linux arm64 (glibc)  | `crepath-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x64          | `crepath-x86_64-pc-windows-msvc.zip`       |

<p align="center">
  <a href="https://github.com/SherinBloemendaal/cursor-repath/actions/workflows/ci.yml"><img src="https://github.com/SherinBloemendaal/cursor-repath/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-yellow.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/rust-1.88%2B-dea584.svg" alt="Rust 1.88+">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-555555.svg" alt="macOS, Linux, Windows">
</p>

## Cursor Repath

**Cursor Repath** (`crepath`) keeps Cursor chat history attached to a project when the folder moves. Repo: [github.com/SherinBloemendaal/cursor-repath](https://github.com/SherinBloemendaal/cursor-repath).

Cursor stores chats against a workspace path. Rename or move that folder and the history stays behind. `crepath` rewrites the local references so the chats follow.

It is not affiliated with Anysphere. It only reads files already on your machine. See [DISCLAIMER.md](DISCLAIMER.md).

Close Cursor before any command that writes. `crepath` reads the native process table, aborts if any Cursor instance of any profile is running (including windows started with `--user-data-dir`), and checks again between steps. If the process table cannot be read, writes are refused. `ls`, `stats`, and `history` never wait on this check. `-n` previews without writing. `-y` skips the single warning prompt.

## Commands

Any command with no arguments opens the picker (space toggles, Enter once, then one validation screen). `crepath` with no command prints colored help. There is no `mg` command.

| Command                                 | What it does                                                                                 |
| --------------------------------------- | -------------------------------------------------------------------------------------------- |
| `crepath mv [FROM] [TO]`                | Repath workspace metadata. Alias: `move`.                                                    |
| `crepath cp [FROM] [TO]`                | Copy: new workspace identity, chats duplicated. Alias: `copy`.                               |
| `crepath save [ID] [TO]`                | Attach an unsaved `Workspaces/<ts>` session to a folder or `.code-workspace`. Metadata only. |
| `crepath split [SOURCE] [TARGETS...]`   | Copy chats from one workspace into separate projects.                                        |
| `crepath combine [TARGET] [SOURCES...]` | Pull chats from several workspaces or chats into one target.                                 |
| `crepath rx [TARGET]`                   | Rebuild registry refs, rewrite leftover path refs, clear caches. Alias: `reindex`.           |
| `crepath ls`                            | Table of workspaces. Alias: `list`. `ls <id>` shows the chat list.                           |
| `crepath rm [TARGET]`                   | Remove workspace metadata, or one chat (including subagents). One confirmation.              |
| `crepath export [TARGET] [FILE]`        | Write a `.crepath` gzip archive.                                                             |
| `crepath import [FILE] [TO]`            | Restore a `.crepath` archive. `TO` attaches it to another folder.                            |
| `crepath history`                       | Local log at `~/.crepath/history.jsonl`. Entries older than 30 days are pruned on write.     |
| `crepath stats`                         | Profiles, workspaces, chats, disk, usage cost, context size, message tokens, models.         |
| `crepath cache clear\|scan\|stats`      | Clear, rescan, or inspect the persistent index behind `ls`, `stats`, and the pickers.        |
| `crepath help [COMMAND]`                | Colored help, or every option of one command.                                                |
| `crepath update`                        | Download and install the latest release for this OS.                                         |
| `crepath github`                        | Open the GitHub repository in the browser.                                                   |

`crepath update` checks `SHA256SUMS`, then replaces the binary in `$CREPATH_INSTALL` or `~/.crepath/bin`.

`crepath github` opens https://github.com/SherinBloemendaal/cursor-repath.

`mv` and `cp` change metadata only. `--project` also moves the real folder. `--project` is refused when the destination parent is missing.

`split` copies by default. `--move` removes the chats from the source.

`combine` copies by default (`--copy`). `--move` removes the chats from the sources. The target can be an existing workspace, or a real folder or `.code-workspace` path that `crepath` creates. Sources are whole workspaces or individual chat ids.

`rx` rebuilds the chat registry from the workspace's `workspace.json`: it fixes stale `workspaceIdentifier` entries, adopts chats that still point at this path from a workspace id that no longer exists, rewrites their old paths, and clears caches.

`ls` columns: dest (present, missing, or none), workspace, kind (folder, code-workspace, unsaved, empty-window, remote), profile, chats, subagents, size, and the first 8 characters of the hash. Missing destinations come first, then the largest workspaces. `--unsaved` limits the table to unsaved sessions. The profile column names the Cursor installation (see [Profiles](#profiles)), plus `/NAME` for a VS Code profile other than the default one.

`stats` only reports what Cursor records: usage cost and requests, the context size at the last turn, and message tokens, each with the number of conversations that carry it. Message tokens are partial: only counts stored inline in the chat are included, and newer chats store tokens per message. `ls` and `stats` open the databases read-only and never create files next to them.

A missing destination is a warning. That item is skipped.

### Shared flags

| Flag                | Effect                                                |
| ------------------- | ----------------------------------------------------- |
| `-n`                | Dry-run. Show the plan and write nothing.             |
| `-y`                | Skip the single warning prompt.                       |
| `--profile NAME`    | Limit the run to one Cursor installation.             |
| `--replace FROM TO` | Batch-rewrite a path prefix.                          |
| `--regex`           | Treat `--replace` FROM as a regular expression.       |
| `--unsaved`         | Include or filter unsaved `Workspaces/<ts>` sessions. |
| `--color WHEN`      | Color output: `auto` (default), `always`, or `never`. |

`auto` colors a terminal and honors `NO_COLOR`, `CLICOLOR_FORCE`, `FORCE_COLOR`, `CLICOLOR=0`, and `TERM=dumb`.

### Command flags

| Flag          | Commands           | Effect                                                |
| ------------- | ------------------ | ----------------------------------------------------- |
| `--project`   | `mv`, `cp`         | Also move or copy the real project folder.            |
| `--move`      | `split`, `combine` | Move the chats instead of copying them.               |
| `--copy`      | `combine`          | Keep the chats in the sources (the default).          |
| `--overwrite` | `import`           | Replace chats that already exist instead of skipping. |
| `--fresh`     | `ls`, `stats`      | Read live data, then refresh the index.               |
| `--full`      | `cache scan`       | Rebuild the index from scratch.                       |

### Examples

```bash
crepath mv --replace /OrbStack/noble/ /OrbStack/resolute/ -y
crepath save 1765558213752 ~/projects/foo
crepath split 5af0e30872454aba2290760e07cae157 ~/projects/api ~/projects/frontend
crepath combine ~/projects/api 7b86a000ce7c6377c39429a3bd7e2080 9d95c4710638ebad3cb7aaa3bec9a067 --move
```

## Index

`ls`, `ls <id>`, `stats`, the pickers, and the split auto-suggest read from a persistent index in `~/.crepath/index.db`. The first `ls` or `stats` for an installation builds it in the foreground. Later runs render from the index at once, print a dim `as of HH:MM` note on stderr, and start `crepath __refresh-index` detached in the background, at most once a minute. That refresh opens Cursor's databases read-only, re-reads only the files whose size or modification time changed (`state.vscdb` and its `-wal`, `storage.json`, and each workspace's `workspace.json` and `state.vscdb`), and reads chat data only for chats whose header changed. `~/.crepath/index.lock` lets one refresh run at a time, and `~/.crepath/refresh.log` keeps its output, trimmed to the newest 64 KB once it passes 256 KB.

Write commands never trust the index: they read live data before and while they write. After a successful write the installation is marked dirty, so the next read refreshes it first.

`--fresh` bypasses the index for one `ls` or `stats` and refreshes it afterwards. `CREPATH_NO_INDEX=1` turns the index off completely. `CREPATH_HOME` moves crepath's state directory (history, backups, index) away from `~/.crepath`.

| Command                                | What it does                                                                                    |
| -------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `crepath cache stats`                  | Index size and schema, last scan and stale sources per installation, and the background status. |
| `crepath cache scan [--profile NAME]`  | Refresh now with progress bars. `--full` rebuilds. `-n` only lists what is stale.               |
| `crepath cache clear [--profile NAME]` | Delete the index, or only one installation's rows. One confirmation. Refused during a refresh.  |

## Split assignment

`split` asks which chats go where. Three modes, in this order:

1. **Auto-suggest (default).** For each chat, touched file paths are mapped to the longest matching target root. A table shows the title, date, and suggested targets. Space toggles targets. Chats with no evidence stay **unassigned** until you pick a target.
2. **All to all.** Every chat is copied to every target.
3. **Manual.** Pick chats per target, with no suggestions.

## Export archive

`export` writes a `.crepath` gzip archive: the `workspaceStorage` directory, the projects directory, `composerHeaders` rows, composer-keyed `cursorDiskKV` rows, the `agentKv` blobs those chats reference, a `storage.json` excerpt, and a manifest with version and checksums. Rows keep their SQLite type, so `NULL` and binary values survive. `export` refuses to overwrite an existing file.

`import` verifies every checksum, rejects unsafe paths and ids, and writes in one transaction. Chats that already exist are skipped unless you pass `--overwrite`. With `TO`, the workspace id is recomputed for that path and the chats' paths are rewritten to it.

## Where Cursor stores it

Workspace ids follow VS Code's formula. A folder hashes its path plus its birth time in milliseconds (rounded on macOS, floored on Windows) or, on Linux, its inode. A `.code-workspace` file or an unsaved `Workspaces/<ts>/workspace.json` hashes its config path, lowercased except on Linux. Paths are normalized first (absolute, no `.`/`..`, no trailing separator), and Windows paths use Cursor's `file:///c%3A/...` URI form.

| Platform | Workspace storage                                             |
| -------- | ------------------------------------------------------------- |
| macOS    | `~/Library/Application Support/Cursor/User/workspaceStorage/` |
| Linux    | `~/.config/Cursor/User/workspaceStorage/`                     |
| Windows  | `%APPDATA%\Cursor\User\workspaceStorage\`                     |

`crepath` also updates `workspace.json`, `globalStorage/storage.json`, the matching rows in `globalStorage/state.vscdb`, and `~/.cursor/projects/`.

Every write command runs as one database transaction with an undo journal in `~/.crepath/backups`: the original image of each row it touches, plus copies of `storage.json` and the workspace files it changes. If a step fails, verification fails, or Cursor starts, everything is rolled back, including folder moves and transcripts. The journal is deleted after a verified run and kept only when a rollback could not finish. Free space is checked up front for the database log, the journal, and any copies.

## Profiles

Every Cursor installation on the machine is a profile: the default user data directory above, any sibling `Cursor*` directory next to it (`Cursor Nightly` becomes `nightly`), and every `~/.cursor-NAME` directory that Cursor was started on with `--user-data-dir ~/.cursor-NAME` (it becomes `NAME`). Each has its own `workspaceStorage`, `globalStorage/state.vscdb`, and `storage.json`. All of them write agent transcripts into the one shared `~/.cursor/projects/`, keyed by folder path.

`ls` and `stats` cover every profile. `stats` counts them and adds a table per profile with its workspaces, chats, subagents, and global database size. `--profile NAME` limits any command to one installation. `--profile NAME/PROFILE` narrows it to one VS Code profile (`userDataProfiles` in `storage.json`) inside it, and a bare VS Code profile name works when only one installation has it.

Write commands never mix installations. Without `--profile` they use the installation that holds the workspace or chat you name, ask in a terminal when several do, and otherwise stop and ask for `--profile`. Sources from different installations are refused, and so is a target that only exists in another installation. When a folder is open in more than one installation, `mv` moves only this installation's transcripts in `~/.cursor/projects/` and copies the rest of that directory, `export` leaves the other installations' transcripts out, and `mv --project` warns that the other installations will point at a missing folder.

## Build from source

Requires Rust 1.88+.

```bash
git clone https://github.com/SherinBloemendaal/cursor-repath
cd cursor-repath
cargo install --path .
```

## License

[MIT](LICENSE)
