# basics

Extract heading-based outline trees from Markdown files.

```sh
ext note.md                 # TUI by default when stdout is a terminal
ext notes/ --tree           # Catppuccin ANSI-styled boxed tree output
ext notes/ --outline        # plain outline output
ext notes/ --json           # JSON output
ext notes/ --json --output=headings.json
ext notes/ --style=never --tree

# backwards-compatible form
ext notes/ --format=tree
ext notes/ --format=outline
ext notes/ --format=json
```

TUI controls:

* `h` / left: previous card
* `l` / right: next card
* `j` / down: nearest card below
* `k` / up: nearest card above
* PgUp / PgDn: page scroll
* Home / End: first / last card
* `q` / Esc: quit

# config

`ext` looks for JSON config in this order:

1. `--config=FILE`
2. `.ext.json` in the current directory or any parent
3. `~/.config/ext/config.json`

Example:

```json
{
  "ignore_heading_paths": [
    ["⛩️"],
    ["⛩️", "参考"],
    ["⛩️", "进步"]
  ],
  "ignore_mode": "subtree"
}
```

`ignore_mode`:

* `subtree`: ignore matched headings and all descendants
* `node`: ignore only matched heading nodes, keeping children

# dev

```sh
just run README.md
just build
just install
```
