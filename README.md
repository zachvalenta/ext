# basics

Extract heading-based outline trees from Markdown files.

```sh
ext note.md                 # tree output by default
ext notes/ --format=outline
ext notes/ --format=json --output=headings.json
ext notes/ --style=always   # force ANSI-styled boxed tree output
ext notes/ --style=never    # plain tree output
```

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
