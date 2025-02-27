# Workspaces

Workspaces help organize large codebases by splitting them into multiple packages with
independent dependencies.

When using the `uv pip` interface, workspace dependencies behave like automatic editable path
dependencies. Using the uv project interface, all of the workspace packages are locked together.
`uv run` installs only the current package (unless overridden with `--package`) and its workspace and
non-workspace dependencies.

## Configuration

A workspace can be created by adding a `tool.uv.workspace` to a pyproject.toml that is the workspace
root. This table contains `members` (mandatory) and `exclude` (optional), with lists of globs of
directories:

```toml
[tool.uv.workspace]
members = ["packages/*", "examples/*"]
exclude = ["example/excluded_example"]
```

If `tool.uv.sources` is defined in the workspace root, it applies to all packages, unless
overridden in the `tool.uv.sources` of a specific project.

## Common structures

There a two main workspace structures: A **root package with helpers** and a **flat workspace**. 

The root workspace layout defines one main package in the root of the repository, with helper packages in `packages`. In this example `albatross/pyproject.toml` has both a `project` section and a `tool.uv.workspace` section.

```
albatross
├── packages
│   ├── provider_a
│   │   ├── pyproject.toml
│   │   └── src
│   │       └── provider_a
│   │           ├── __init__.py
│   │           └── foo.py
│   └── provider_b
│       ├── pyproject.toml
│       └── src
│           └── provider_b
│               ├── __init__.py
│               └── bar.py
├── pyproject.toml
├── README.md
├── uv.lock
└── src
    └── albatross
        └── main.py
```

In the flat layout, all packages are in the `packages` directory, and the root
`pyproject.toml` defines a so-called virtual workspace. In this example `albatross/pyproject.toml` has only a `tool.uv.workspace` section,
but no `project`.

```
albatross
├── packages
│   ├── albatross
│   │   ├── pyproject.toml
│   │   └── src
│   │       └── albatross
│   │           ├── __init__.py
│   │           └── foo.py
│   ├── provider_a
│   │   ├── pyproject.toml
│   │   └── src
│   │       └── provider_a
│   │           ├── __init__.py
│   │           └── foo.py
│   └── provider_b
│       ├── pyproject.toml
│       └── src
│           └── provider_b
│               ├── __init__.py
│               └── bar.py
├── pyproject.toml
├── README.md
└── uv.lock
```
