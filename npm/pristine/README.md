# pristine

A language-agnostic reclaimable-space finder and cleaner. It finds the build output, dependency
directories and caches that a tree can regenerate — `node_modules`, `target`, `.venv`, `obj`,
`_build`, `vendor` — reports what they cost, and deletes the ones you confirm.

```sh
npx @agentender/pristine ~/repos
```

or install it:

```sh
npm install -g @agentender/pristine
```

## What this package is

A wrapper over the prebuilt binaries the `pristine-cli` crate builds, not a reimplementation.
Installing it pulls in exactly one `@agentender/pristine-<platform>` package — npm picks it from
the `os` and `cpu` each one declares — and the `pristine` command runs the binary inside it.

There is no postinstall step and nothing is downloaded at install time, so this works offline,
behind a proxy, and in environments that disable install scripts.

Prebuilt platforms: `darwin-arm64`, `darwin-x64`, `linux-x64-gnu`, `linux-arm64-gnu`. Anywhere
else, build from source with `cargo install pristine-cli`.

## Links

Source, documentation and issues: <https://github.com/AgentEnder/pristine>.
