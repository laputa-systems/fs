# fs

`fs` copies or synchronizes files and directories on macOS and Linux.

```sh
fs cp [OPTIONS] SRC DST
fs sync [OPTIONS] SRC DST
```

`cp` overlays the source onto the destination and keeps destination-only
entries. `sync` removes destination-only entries after the source has been
copied successfully.

## Install

Download a matching archive from [GitHub Releases](https://github.com/laputa-systems/fs/releases),
unpack it, rename the executable to `fs`, and put it on your `PATH`. Prebuilt
releases target Apple Silicon macOS and x86_64 or aarch64 Linux on musl; Linux
has dynamic and static variants. The current workflow does not build an Intel
macOS archive.

## Use

```sh
fs cp ./project ./backup
fs sync --check=hash ./photos ./archive
```

Directories are copied as tree roots; their basename is not added to the
destination. Trailing slashes have no effect. The destination's parent
directories must already exist, though the final destination may be absent.
The same source and destination is a no-op. Overlapping directory trees fail
before mutation.

Only regular files, directories, and symlinks are supported. Intermediate
symlinks are rejected and symlinks are never followed. A final source symlink
is copied as a symlink. Type conflicts fail rather than replacing one kind of
object with another.

## Options

| Option | Effect |
| --- | --- |
| `-n`, `--dry-run` | Report planned operations without changing the filesystem or creating temporary objects. |
| `-v`, `--verbose` | Print operations to stdout; real runs report completed mutations. |
| `-j N`, `--jobs N` | Set the worker count; `N` must be positive. By default it is chosen automatically and clamped to 2–8. |
| `--check=metadata` | Compare file size, mode, and destination-normalized modification time. This is the default. |
| `--check=hash` | Compare file contents with BLAKE3. |
| `--durable` | Synchronize file data and metadata before publication and the parent directory after namespace changes. |
| `--cross-file-systems` | Allow traversal across filesystem mount boundaries. |
| `--no-progress` | Disable interactive progress output. |
| `-h`, `--help` | Show command syntax and options. |
| `-V`, `--version` | Show the version. |

Diagnostics and progress use stderr. Progress is shown only when stderr is a
terminal. Normal publication is atomic; `--durable` adds synchronization for
stronger crash durability.

See [`LICENSE`](LICENSE) for the MIT license.
