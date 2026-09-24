# Disclaimer

Cursor Repath (`crepath`) is an independent open-source tool. It is **not affiliated with, endorsed by, or sponsored by Anysphere, Inc.**, the company behind Cursor. "Cursor" is used only to describe what this tool works with.

## What it does

Cursor keeps chat history and workspace state in files on your machine. `crepath` reads and updates those local files.

- `mv` and `cp` repath a workspace. With `--project`, they also move the real folder.
- `save` attaches an unsaved `Workspaces/<timestamp>` session to a folder or a `.code-workspace` file.
- `split` copies chats from one workspace into separate projects. `--move` removes them from the source.
- `combine` copies chats from several sources into one target. `--move` removes them from the sources. There is no `mg` command.
- `rx` (alias `reindex`) rebuilds registry references, rewrites leftover path references, and clears stale caches.
- `ls`, `rm`, `export`, `import`, `history`, and `stats` list, remove, archive, restore, and summarize that local data.

## What it does not do

- It does not contact Cursor servers or call any Cursor API.
- It does not send your data anywhere. Everything stays on your machine.
- It does not read, modify, or redistribute Cursor's application code.
- It does not bypass licensing, authentication, or any other access control.

## Your data, your responsibility

`crepath` edits Cursor's local databases. The format is undocumented and can change between Cursor releases. Before it writes, it takes a backup so an aborted run can be restored. `-n` previews a command without writing. `-y` skips the single warning prompt. Even so:

- Close Cursor before any command that writes. `crepath` aborts if Cursor is running.
- Preview with `-n` first.
- Keep your own backup of anything you cannot afford to lose.

`.crepath` archives can contain code, credentials, or other people's messages. Treat an export like the source it came from, and do not share it without permission.

## No warranty

This software is provided "as is", without warranty of any kind. See [LICENSE](LICENSE) for the full terms.
