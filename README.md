# ren

`ren` is a fast bulk file renamer, sibling of [`rep`](https://github.com/gechr/rep).

Where `rep` rewrites file *contents*, `ren` rewrites file *names*. Plain and regex find/replace, smart preserve-case rewrites, an interactive preview, glob filters, dry runs, listing, and multiple `-e/--expression` rewrites in a single pass.

## Install

```shell
cargo install --git https://github.com/gechr/ren
```

## Usage

<img src="assets/help.png" alt="help" width="700">

## Examples

```sh
# Replace `foo` with `bar` anywhere in basenames (cwd, files only)
ren foo bar

# Recurse into subdirectories
ren -R foo bar

# Also rename matching directories
ren --include-dirs foo bar

# Restrict to .rs files
ren -f rs old new

# Match the stem only; leave extensions alone (api_v1.json -> api_v2.json)
ren -E v1 v2

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

# Number every file in cwd: 01_alpha.txt, 02_beta.txt, ... (smart default)
ren --counter

# Custom counter format with zero-padding and a path scope
ren --counter='{n:03}_' src/

# Lowercase every basename in cwd
ren --lower

# Compose: find/replace, then lowercase, then counter
ren --counter --lower foo bar
```

## Transforms

When `<find> <replace>` alone isn't enough, transforms layer over the find/replace stage. They're optional, compose with each other, and make `<find> <replace>` itself optional too - so `ren --counter` is a valid invocation.

| Flag                      | Effect                                         |
| ------------------------- | ---------------------------------------------- |
| `-L`, `--lower`           | lowercase the basename (mutex with `--upper`)  |
| `-U`, `--upper`           | uppercase the basename                         |
| `-A`, `--prepend <str>`   | prepend a literal string                       |
| `-a`, `--append <str>`    | append a literal string                        |
| `-c`, `--counter[=<fmt>]` | prepend a sequential counter                   |

Counter format: `{n}` substitutes the 1-based index; `{n:0WIDTH}` zero-pads to `WIDTH` digits. Anything else passes through, so `[{n:02}]-`, `chapter-{n:03}_` etc. work as you'd expect. Use `--counter` alone for a smart per-directory default: `01_` below 100 entries, `001_` at 100+, `0001_` at 1000+, and so on. Use `--counter=FORMAT` to customize.

The pipeline runs in fixed canonical order regardless of argv order:

1. find/replace (positional or `-e`)
2. `--lower` *or* `--upper`
3. `--append`
4. `--prepend`
5. `--counter`

`-E/--no-extension` scopes the entire pipeline to the stem; the extension is reattached afterward. Counter indexes reset per parent directory - with `--recursive`, each directory starts at `01_` for the smart default. Files filtered out by find/replace don't consume a counter slot.

## Preview keymap

`ren --preview` walks each plan entry and asks for a decision:

| Key | Action                                    |
| --- | ----------------------------------------- |
| `y` | accept this rename                        |
| `n` | reject this rename                        |
| `A` | accept this and all remaining renames     |
| `q` | stop prompting and apply accepted renames |
| `←` | go back and revise the previous decision  |
| `→` | skip this rename                          |

Combine with `--dry-run` to walk the prompts without touching the filesystem.
