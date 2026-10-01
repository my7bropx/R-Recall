# recall

A terminal knowledge base with **Vim-style modal editing** and SQLite FTS5
full-text search. Store commands, notes, and tools in a local database and pull
them up fast — from an interactive TUI or straight from the command line.

## Build

```sh
cargo build --release
# binary at ./target/release/recall
```

`rusqlite` is built with the `bundled` feature, so SQLite is compiled from
source — no system `libsqlite3` needed. A C compiler (`cc`/`gcc`) must be
present.

## Install system-wide

```sh
./install.sh                        # builds --release and installs to /usr/local/bin
                                     # (asks for sudo only if that's not writable)
PREFIX=$HOME/.local ./install.sh    # per-user install, no sudo — make sure
                                     # $HOME/.local/bin is on your PATH
```

`recall` is then on your `PATH` from any directory. To remove it:

```sh
./uninstall.sh                      # match with the same PREFIX you installed with
```

Uninstalling only removes the binary — your database at
`$HOME/.local/share/recall/recall.db` is left untouched.

## Command-line usage

```sh
recall                                  # launch the TUI
recall nmap ping sweep                  # shorthand for `recall show` — search and read
recall cmd nmap -p-                     # print the resolved command, script-safe
recall copy full tcp sweep              # resolve (prompting for anything open) and copy it
recall run docker system prune          # resolve, confirm, execute in your shell
recall add -t "nmap scan" -c "nmap -p- <ip>" -C command --tags "recon,nmap"
recall search nmap                      # legacy plain output (pipe-safe: | head works)
recall db-path                          # where the database lives
recall import notes.md                  # bulk-import Markdown (see below)
```

Every subcommand's `--help` documents its flags. The rest of this README covers
the TUI; see **Finding and using a command** below for the full CLI story
(placeholders, variables, the built-in pack, danger detection, shell
integration).

## The TUI is modal (like Vim)

The interface has four modes. The current mode is shown at the bottom-left
(`-- NORMAL --`, `-- INSERT --`) and the `:`/`/` line appears there too.

### Normal mode — browsing the list

| Key | Action |
| --- | --- |
| `j` / `k` | move down / up |
| `gg` / `G` | jump to top / bottom |
| `gt` / `gT` | next / previous tab (ALL → CMD → NOTE → TOOL → FAV, wraps) |
| `Ctrl-d` / `Ctrl-u` | half-page down / up |
| `l` / `Enter` | open the full-screen view |
| `o` | new entry |
| `e` | edit the selected entry |
| `dd` | delete the selected entry |
| `u` | undo the last change (delete, edit, favorite, or `:g` batch) |
| `Ctrl-r` | redo |
| `yy` / `+y` | yank the entry's whole content to the clipboard |
| `Y` | yank just the **command**, placeholders silently filled from pinned variables |
| `v` / `V` | Visual / Visual-Line selection in the side preview pane |
| `f` | toggle favorite (★) |
| `/` | search — type to filter live (FTS5 prefix match) |
| `n` / `N` | jump to next / previous match |
| `:` | command mode |
| `?` | help overlay |
| `q` | quit |

**Arrow keys are deliberately inert.** Pressing ←↓↑→ in any normal mode does
nothing except print *Arrows are disabled — use h j k l*. Typing in a form is
unaffected.

### Normal mode — full view

`j`/`k` scroll, `gg`/`G` jump to ends, `Ctrl-d`/`Ctrl-u` half-page, `yy`/`+y`
yank the whole entry, `f` favorite, `q`/`h`/`Esc` back to the list.

### Visual and Visual-Line selection

`v` enters **Visual** (character-wise) selection and `V` enters **Visual
Line** selection, anchored at the current cursor line. This works in both
places an entry's content is shown: the side **Preview** pane on the list
screen (press `v`/`V` there — it starts fresh at the top of that entry each
time) and the full-screen view (`l`/`Enter`, where it picks up from wherever
you've already scrolled to). While selecting:

| Key | Action |
| --- | --- |
| `j` / `k` / `gg` / `G` | extend the selection up/down (view auto-scrolls) |
| `h` / `l` | move within a line (character-wise `v` only) |
| `y` / `+y` | yank the selection to the clipboard, then return to Normal |
| `v` / `V` / `Esc` | exit the current visual mode without yanking |

The yank copies **exactly** the highlighted text — no trailing newline, no
extra lines above or below, and character-wise selection copies only the
selected columns (inclusive of both ends, like Vim). `+y` is the same
operation as `y`: this app has a single clipboard register, the system
clipboard, so there's no distinct default-register/`"+`-register split to
worry about.

### Insert mode — inside the add/edit form

Reached with `i` or `a` on a form. Type into the focused field; `Esc` returns to
Normal. `Enter` inserts a newline in the Content field (and moves to the next
field elsewhere). `Tab`/`Shift-Tab` move between fields.

### Normal mode — inside the form

`i`/`a` start typing, `Tab` or `j`/`k` move between fields, `h`/`l` cycle the
category, `:w` saves, `:q` cancels, `Esc` also cancels.

A form with unsaved changes shows `[+]` in its title and refuses to close: `:q`,
`Esc`, and `Ctrl-C` all report *No write since last change*. Use `:w` to save or
`:q!` to discard — the same contract as Vim.

## Command mode ( `:` )

| Command | Action |
| --- | --- |
| `:w` | save the current form (stays open) |
| `:q` | form → cancel (refused if unsaved) · view → back · list → quit |
| `:q!` | close the form, discarding unsaved changes |
| `:wq` / `:x` | save the form and close |
| `:d` / `:delete` | delete the selected entry |
| `:u` / `:undo` | undo the last change |
| `:redo` | redo |
| `:g/pattern/d` | delete every visible entry matching the pattern |
| `:g/pattern/fav` \| `unfav` | (un)favorite every match |
| `:theme [name]` | switch theme live, or show the current one |
| `:new` | new entry |
| `:e` / `:edit` | edit the selected entry |
| `:sort [key] [!]` | sort by `title`/`updated`/`created`/`category`/`favorite`; `!` reverses |
| `:cat all\|command\|note\|tool` | filter by category (same as `gt`) |
| `:fav` | toggle favorite on the selected entry |
| `:favorites` | show favorites only (toggle) |
| `:noh` | clear the active search filter |
| `:editor` | open `$EDITOR` for the content field (in a form) |
| `:import <path>` | import a Markdown file live |
| `:set <name> <value>` | pin a variable (see **Placeholders and variables**) |
| `:unset <name>` | remove a pinned/remembered variable |
| `:vars` | list variables (`*` marks an unpinned, remembered one) |
| `:help` | help overlay |

## Highlighting

Entry content is syntax-highlighted in both the preview and the full view:
fenced code blocks render in green, `#` code comments dim, Markdown headings
bold-yellow, `` `inline code` `` and `**bold**` styled, and bullets accented.
In the list, the characters your search terms matched are highlighted, and
favorites are marked with ★.

## Undo and redo

`u` undoes, `Ctrl-r` redoes, 200 steps deep. Deletes, edits, favorite toggles
and `:g` batches all participate.

* An undone delete is re-inserted with its original timestamps and favorite flag
  intact (SQLite assigns a fresh row id, but every visible field matches).
* An undone edit is written back over the existing row, original `updated_at`
  included — so a reverted edit leaves no trace. If that row was deleted after
  the edit, the undo re-creates it instead of failing.
* A `:g/pattern/d` that removed forty entries is **one** undo step, not forty.
* Making a new change discards the redo branch, exactly as Vim does.

Only saved changes enter the stack: editing a form and pressing `:q!` discards
without touching it. Bulk operations that add rows — `:import`, `recall
import`, `recall pack sync`, `recall tldr sync` — are **not** on the undo
stack (there is no single row to snapshot); each tags what it adds (`--tag`,
`src:pack`, `src:tldr`) so you can review or bulk-remove with `:g/tag/d` /
`recall pack restore` instead, and every real schema change is backed up
first regardless (see **Storage**).

## Bulk operations

`:g/pattern/cmd` applies a command to every entry matching `pattern`:

```
:g/deprecated/d        delete all matches
:g/kerberos/fav        favorite all matches
:g/kerberos/unfav      unfavorite all matches
```

The pattern goes through the same FTS5 search as `/`, and — like Vim's `:g`,
which acts on the current buffer — it only touches entries **visible under the
active tab and favorites filter**. So `gt` to CMD first, and `:g/nmap/d` leaves
your NOTE entries alone. Every batch is a single undo step.

## Finding and using a command

Entries now carry a dedicated **tool** (`nmap`, `git`, …) and **command**
field alongside the free-text content, shown in the list as `tool › title`. A
handful of CLI verbs are built around getting that command onto your prompt
as fast as possible, with placeholders filled in along the way.

```sh
recall show gobuster dir           # search and read: tool, command, tags, ⚠ if destructive
recall cmd nmap -sV                # print just the resolved command — never prompts, script-safe
recall copy full tcp sweep         # resolve (prompting for anything still open) and copy it
recall run docker system prune     # resolve, confirm, execute right here in your shell
```

`show`/`cmd`/`copy`/`run` all take the same query language:

```
nmap ping sweep            plain words — FTS prefix match, typo-tolerant
"port scan"                a quoted phrase
tool:nmap  t:git           only entries about that tool
tag:recon  #recon          only entries carrying that tag
cat:cmd|note|tool          only that category
is:fav  is:danger          favorites / commands flagged destructive
src:pack|user|import|tldr  where the entry came from
-sV  --script  -p-         flag-shaped words match as exact substrings
```

Typos are corrected against your own indexed words (`gobsuter` → `gobuster`);
if nothing matches every word, results with *some* of the words are shown
instead — either way you're told what happened, on stderr.

### Placeholders and variables

A command's placeholders — `{{target}}`, `{{wordlist:default/path}}`, or the
`<name>` style already used throughout your own notes — get filled in three
ways, in order: an explicit answer, a **pinned** variable, or a few safe
built-ins (`lhost`, `subnet`, `iface`, `date`, `time`, `cwd`, `home` — `lhost`
and `iface` are detected from your actual network config, preferring a VPN
interface like `tun0` when one exists). Anything still open is either left as
written (`cmd`, and `show`/anywhere non-interactive) or prompted for on stderr
(`copy`/`run`/`pick`), and what you type is remembered as that placeholder's
suggestion next time, without ever being applied silently unless you pin it.

```sh
recall set target 10.10.11.5    # pin — every {{target}}/<ip>/<host>/<rhost>... fills silently
recall vars                     # list what's pinned or remembered
recall unset target
```

Related placeholder names share one variable automatically — pinning `target`
also fills `<ip>`, `<host>`, `<rhost>`, `<victim>`; `user`↔`username`,
`pass`↔`password`, `lhost`↔`attacker_ip`, and so on.

### Destructive commands

Anything that looks like it can destroy something — `rm -rf`, `dd of=/dev/…`,
`git push --force`, `terraform destroy`, `docker system prune`, a dropped
database, and quite a bit more — is flagged automatically (shown as `⚠` in
lists and the TUI). `recall run` requires typing the word `yes` (not just
`y`) for a flagged command, always refuses to run anything without a real
terminal to confirm in, and never runs on its own.

### The picker (`recall pick`, and Ctrl-G at your prompt)

```sh
recall init zsh   >> ~/.zshrc     # or bash / fish
recall init bash  >> ~/.bashrc
recall init fish | source          # or add to config.fish
```

Reopen your shell (or `source` the file) and **Ctrl-G** opens a fuzzy picker
over whatever you've already typed; Enter fills in any remaining placeholders
right there and drops the finished command on your line, ready to edit or
run. `recall pick` also works standalone.

### Everything else

```sh
recall tools                # tools by how many entries cover them
recall tags                 # tags by frequency
recall stats                # counts, sources, coverage, database size
recall doctor [--fix]       # integrity + search-index health check
recall backup [path]        # a timestamped copy of the database file
recall export               # every entry as JSONL on stdout — grep it, jq it, back it up as text
```

## The built-in knowledge pack

Alongside your own entries, `recall` ships a curated set of tool/command
reference entries — the kind of thing you'd otherwise be hunting for across a
dozen cheat sheets. It never overwrites something you've edited, and never
brings back something you've deleted:

```sh
recall pack sync            # add new / update untouched built-in entries
recall pack sync --dry-run  # see what would happen first
recall pack restore         # un-delete: forget what you removed, next sync brings it back
```

Sync compares content hashes: a built-in entry you never touched updates
silently when the pack does; one you edited is left exactly as you left it
forever, even as the pack around it changes. Built-in entries show up with
`src:pack` and rank slightly below your own (`src:user`) at equal relevance.

### Importing example commands from tldr-pages

If you have a local [tealdeer](https://github.com/tealdeer-rs/tealdeer) cache
(`tldr --update`), `recall` can pull its examples in too — **this never runs
on its own**, since a full sync adds tens of thousands of mostly-generic
entries and would swamp a curated personal database:

```sh
recall tldr status                  # is a cache found, and where
recall tldr sync --tool ffmpeg      # just one tool
recall tldr sync --dry-run          # see the scale before committing
recall tldr sync                    # common + linux pages, skips anything already covered
```

## Themes

**Safelight** is the default — a darkroom palette: warm near-black substrate,
silver-gelatin text, and accents drawn from dichroic enlarger filters and
darkroom chemistry (fixer green, stop-bath red, toner violet, safelight amber).
Also available: `default` (plain ANSI), `gruvbox`, `catppuccin`, `nord`,
`tokyonight`.

```sh
recall                          # safelight
recall --theme gruvbox          # per-run
RECALL_THEME=nord recall        # or via the environment
```

`:theme <name>` switches live; bare `:theme` reports the current one and lists
the rest.

Every color is defined twice — an exact 24-bit RGB value and an ANSI-16
fallback. Truecolor is auto-detected from `COLORTERM`; force it either way with
`--truecolor true|false` or `RECALL_TRUECOLOR=0|1`. On a terminal without
truecolor the UI falls back to your terminal's own 16 colors rather than
rendering approximations.

Themes paint their own background so they read as themselves regardless of your
terminal's colors. If you use a transparent or custom terminal background you'd
rather keep, disable it with `--background false` or `RECALL_BG=0` (the plain
`default` theme never paints).

### Highlighting

Entry content is rendered with the theme's Markdown rules rather than one flat
color:

* **Heading levels are colored individually** — Safelight runs amber, yellow,
  cyan, green, magenta, violet for h1–h6, with the `#` marks dimmed.
* Fenced code bodies, `` `inline code` `` and `#` comments inside code blocks
  each get their own color; comments are italic.
* `**bold**`, `*italic*`, bullets, blockquotes, and `[links](url)` are styled,
  with link destinations dimmed and the text underlined.
* Category badges, the ★ favorite marker, borders, selection, and search-match
  highlighting all come from the same palette.

## Search

Search is backed by SQLite's **FTS5** full-text index over six columns —
title, tool, tags, keywords, command, content — each weighted differently for
`bm25` relevance (a hit on the title or tool outranks the same word buried in
a content paragraph). Terms are matched as **word prefixes** and combined
with AND, so `kerb` finds *kerberoast* and `tmux pane` finds entries
containing both. The `tool:`/`tag:`/`cat:`/`is:`/`src:` filters and
flag-shaped words (`-sV`, `-p-`) described above work the same way here and
from the CLI.

If nothing matches every word, misspelled words are corrected against your
own indexed vocabulary first (`gobsuter` → `gobuster`, edit-distance ≤ 2);
failing that, entries matching *some* of the words are shown; failing that, a
plain substring scan. A query with no alphanumeric characters (for example
`-p-`) goes straight to the substring scan.

Results are also nudged by how often you've actually used an entry (`Y`,
`recall cmd`/`copy`/`run`/picking it all count as a use) and whether it's a
favorite, so the command you reach for daily naturally floats up.

`n` / `N` cycle through matches. Because the match set survives `:noh` (or `Esc`,
which clears the *filter* but not the search), you can search, restore the full
list, and still jump between hits with `n` — like Vim.

The index is maintained by SQL triggers on insert/update/delete, so it is never
stale and never needs a manual reindex.

## Importing Markdown

`recall import <file.md>` turns a notes file into entries. It is built for
real, messy files — several documents pasted together, chat exports, PDF
dumps — so every decision is a named rule, and the run prints how often each
one fired.

```sh
recall import MASTER.md                 # import everything (skips duplicates)
recall import MASTER.md --dry-run       # parse and report, write nothing
recall import MASTER.md --flagged-only  # only CRITICAL / IMPORTANT sections
recall import MASTER.md --tag master    # attach an extra tag to every entry
recall import MASTER.md --per-command   # also one entry per annotated command (below)
```

```
Imported 2710 entries (701 skipped, 0 duplicates) from MASTER.md
  headings   3,469 found (100 plain-text like "3️⃣ Title" / "Step 4 — Title"); 4 list items …
  cleaned    416 page header/footer lines removed, 6 ```markdown wrappers unwrapped, …
  entries    1,812 command · 893 note · 5 tool — tool identified on 1,532
  shaped     255 titles made unique with their parent heading, 69 repeated sections dropped …
```

**Where entries start and end.** An entry is a heading plus the text up to the
next heading of any level; a heading with no body of its own is a container and
its children become the entries.

* Headings are `#` to `######`, `# ====` / `# TITLE` / `# ====` banners, and
  plain-text ones such as `3️⃣ Strong passphrase` or `Step 14 — Scan files` (these
  count once they clearly head a body; a run of numbered lines with nothing
  under them stays a list).
* Code fences are paired the way a person reads them, so one bad fence can't
  turn the rest of the file into "code": a ```` ```markdown ```` wrapper is
  unwrapped, a stray fence is dropped, an unclosed block is closed where it
  visibly ends. A `#` line inside code is a comment. Outside code it is a
  heading — unless it sits in a run of commands or comments (an unfenced
  script).
* A "heading" a converter made out of a list or table item (`Good systems
  use:` / `## PBKDF2`) is turned back into a list item.
* Page furniture from a printed web page or chat (a date/title header and a
  URL + `6/103` footer, repeated on every page) is removed, and the printed
  title is kept as context. Link-only tables of contents are dropped; a
  "Quick Reference Card" with a cheat sheet in it is not.
* A section longer than ~6,000 characters is split at paragraph breaks (never
  inside a code block) into `Title (1/3)`, `(2/3)`, … instead of being truncated.

**Titles.** Markup, emoji and numbering (`3.1`, `2)`, `1️⃣`, and a bare `2 …`
when its siblings count 1, 2, 3) are stripped. A generic title (`Examples`,
`Best Practices`) or one shared by several entries is prefixed with its parent
heading — `Docker — Best Practices` — and only entries that are still identical
get a `(2)`. The same section pasted twice is stored once.

**Category, tool, command.**

* `command` — the body has a fenced shell (or `vim` / `tmux` / `awk`) block in
  which some line would be typed (so `cd ~` and `source ~/.bashrc` count, while
  an ssh config — `Host myserver`, `Port 22` — or a block of only comments does
  not); or a key table in a vim/tmux section (`dd   delete line`); or command
  lines make up a real share of it (a quarter of the text, or three lines) —
  unfenced, or as `` - `cmd` - description `` bullets and table rows. A stray
  `cat`/`sudo` line in an essay does not. Code in other languages (`python`,
  `yaml`, …) and a bare fence used as a text box are notes.
* `tool` — a heading that names the program (`nmap (Port Scanning)`, `# awk`),
  else the program that most of the entry's commands run (every stage of a
  pipeline counts; `echo`/`cat` only win alone), else empty. It never falls back
  to "whatever came first", and keybindings (`C-h`), `EOF` and config
  directives (`Host myserver`) are never programs. Which words count as
  programs is learned from the file's own shell blocks.
* `tool` (category) — a heading about tools/frameworks/suites with no command in it.
* A command that looks destructive is flagged, checking every block.

**Per-command entries (`--per-command`).** Cheat sheets annotate every command
with what it does — and those words are what you type when searching. With this
flag each annotated command also becomes an entry of its own, next to its
section: the title is the description, `command` is that one command, and the
tool, tags and danger flag are worked out for it alone. Recognised forms:

```text
# List files with details        ls -la    # List files with details      - `:noh` - Clear highlighting
ls -la                                                                    | `dd` | Delete line |
                                 Ctrl-b d   Detach      (vim/tmux key tables, `" comment` in ```vim blocks)
```

Options on their own (`-l`, `--show`), regex/glob notation (`*.log`, `[a-z]{2,}`),
lone paths (`/etc/passwd`) and config directives are not commands and are skipped.
An identical description + command in two places is stored once, and a section
that already *is* the one command gets no duplicate. On a 62,000-line notes file
this adds ~6,000 entries; `recall cmd` then prints a single copy-ready command
for about two thirds of queries instead of a whole section (median 1 line, was 14),
while finding the right entry stays about as good as before. It is off by default
— sections alone are a smaller, quieter database — and the TUI's `:import` does not
use it.

**Tags and keywords.** Tags: the top-level banner (`linux`, `git`, `crypto` …),
the section, `critical` / `important`, plus any `--tag`. Search keywords come
from the heading path (`linux`, `file operations`), so a query can hit an
entry through its context as well as its own title.

**De-duplication.** Import never stores data that already exists (same title +
content), so importing the same file repeatedly is a no-op, and the result for
a given file is deterministic.

## Storage

The database lives at `$HOME/.local/share/recall/recall.db`, in WAL mode.
Older databases migrate automatically on first open, in one transaction:
existing columns and rows are never rewritten, new columns (`tool`, `command`,
`keywords`, `danger`, usage tracking, provenance) are added and back-filled
from your existing content, and the search index is rebuilt to match. If the
database isn't empty, a full pre-migration backup is written alongside it
first (`recall.db.pre-v2-<timestamp>.bak`) and the path is printed — this only
ever happens once per database. `recall doctor` checks integrity and index
health at any time; `recall backup` makes an on-demand copy.

Measured on a 3,200-entry database: each search ~3 ms, a no-op re-import (the
idempotency check alone) ~40 ms, a no-op `pack sync` ~35 ms.

## Clipboard

`yy`, `Y`, `+y`, yanking a Visual/Visual-Line selection, and `recall copy` all
shell out to `xclip`, then `xsel`, then `wl-copy` (first found). Install one
for clipboard support.
