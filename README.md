# ren

`ren` is a fast bulk file renamer, sibling of [`rep`](https://github.com/gechr/rep).

Where `rep` rewrites file *contents*, `ren` rewrites file *names*. Plain and regex find/replace, smart preserve-case rewrites, an interactive preview, glob filters, dry runs, listing, and multiple `-e/--expression` rewrites in a single pass.

## Install

```shell
cargo install --git https://github.com/gechr/ren
```

## Usage

```text
ren [options] <find> <replace> [<path>...]
```

By default `ren` operates on files in the current directory only. Pass `-R/--recursive` to descend into subdirectories, and `--include-dirs` to admit directory entries into the rename plan.

Run `ren -h` for the short help, or `ren --help` for the full help with examples.

## Examples

```sh
# Rename files in cwd: foo* -> bar*
ren foo bar

# Recurse into subdirectories
ren -R foo bar

# Also rename matching directories
ren --include-dirs foo bar

# Restrict to .rs files
ren -f rs old new

# Match the stem only; leave extensions alone
ren -E txt notes

# Smart-rename across case variants
#   foo_bar -> hello_world, FooBar -> HelloWorld, FOO_BAR -> HELLO_WORLD, ...
ren --smart foo_bar hello_world

# Regex rename: replace test_ prefix with spec_
ren --regex '^test_' 'spec_'

# Interactive preview before applying
ren --preview foo bar

# Plan only, don't touch the filesystem
ren --dry-run foo bar

# Preview the plan, accept/reject entries, then print (not apply)
ren --preview --dry-run foo bar

# Apply multiple replacements in a single pass
ren -e foo bar -e baz qux

# Print matching paths only (no rename)
ren -l foo

# Case-only rename (works on case-insensitive filesystems too)
ren tmp TMP
```

## Preview keymap

`ren --preview` walks each plan entry and asks for a decision:

| Key  | Action                                   |
| ---- | ---------------------------------------- |
| `y`  | accept this rename                       |
| `n`  | reject this rename                       |
| `A`  | accept this and all remaining renames    |
| `q`  | quit immediately, applying nothing       |
| `<-` | go back and revise the previous decision |

Combine with `--dry-run` to walk the prompts without touching the filesystem.

## Case-insensitive filesystems

On case-insensitive filesystems (macOS APFS, Windows NTFS, etc.) a direct `fs::rename("tmp", "TMP")` is silently a no-op - both names refer to the same dirent. `ren` always renames via a unique intermediate path (`<basename>.ren-<pid>-<counter>`), so a case-only rename like `tmp -> TMP` actually flips the dirent casing. The temp-hop is unconditional, so the algorithm is identical on case-sensitive filesystems too.

The same mechanism handles chains (`a -> b, b -> c`), cycles (`a -> b, b -> a`), and nested directory renames (`foo/bar.txt -> foo/baz.txt` together with `foo -> qux`) without special cases. Plan entries are grouped by path depth and applied deepest-first.

## Limitations

- **`-E/--no-extension` follows `Path::extension()` semantics.** Only the *last* `.ext` is treated as the extension: `archive.tar.gz` has stem `archive.tar` and extension `gz`. Leading-dot files like `.bashrc` have no extension, so `-E` matches the whole basename. A trailing-dot name like `archive.` round-trips the empty extension.
- **Approximate Unicode case folding.** Collision detection lowercases via `String::to_lowercase`, which is *not* full Unicode case folding (Turkish dotted-I, German `ß`/`SS`, etc.). Adequate for ASCII-only filenames, which are the common case.
- **Non-UTF-8 basenames are warned and skipped.** A non-UTF-8 basename triggers a warning to stderr and is omitted from the plan, matching `rep`'s permissive philosophy.
- **Per-failing-depth rollback only.** If `apply_plan` fails at depth `d`, depths deeper than `d` are already committed and stay applied; depth `d` itself is rolled back. The error message states this so the partial-state shape is clear. There is no global undo in v1.
- **Ctrl-C mid-apply may leave orphan temp files.** Pattern: `*.ren-<pid>-*`. Recover with `find . -name '*.ren-*' -delete`.
- **`EXDEV` errors are surfaced verbatim.** Cross-mount renames don't normally happen because `ren` only changes basenames, but a symlinked path could trigger one.

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
