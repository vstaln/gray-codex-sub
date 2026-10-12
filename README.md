<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-codex-sub</h1>
<p align="center">Run gray turns on your ChatGPT plan — a Codex-subscription provider sidecar.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-codex-sub/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

`gray-codex-sub` lets gray run turns on your ChatGPT plan instead of an API
key. It is a protocol-1.2 provider sidecar: gray owns the agent loop, tools,
approvals, and compaction — the plugin drives pooled `codex app-server`
children, the Codex CLI's own engine, behind a per-turn loopback relay.

## Requirements

- Codex CLI on `PATH` (>= 0.154), or set
  `GRAY_CODEX_SUB_COMMAND=/path/to/codex`.
- Signed in with the CLI against the plugin's Codex home:

  ```sh
  CODEX_HOME=$HOME/.gray/codex-home codex login
  ```

  Or point `CODEX_HOME` at your existing Codex login (e.g. `~/.codex`).

## What the plugin does and does not do

- **Reads no tokens.** It never opens `$CODEX_HOME/auth.json` (existence
  check only, for a fast error message) and runs no OAuth flow of its own.
  `codex login` runs only in your terminal; the CLI owns and rotates its
  tokens.
- **Calls no internal endpoints.** Every request — chat, sign-in check,
  model list, usage — goes through the official `codex app-server`
  process (`account/read`, `model/list`, `account/rateLimits/read`,
  `thread/*`, `turn/*`). Nothing is sent to `chatgpt.com/backend-api`.
- **Sends no spoofed client identity.** The app-server handshake names
  `gray-codex-sub` as the client. No borrowed client IDs, no
  `chatgpt-account-id` headers.
- **Keeps the session locked down.** Threads run `approvalPolicy: never`
  with a `read-only` sandbox, ephemeral (nothing on disk), in a
  per-thread temp staging dir. Any other server→client request is
  refused.
- **Separate Codex home.** Unless you set `CODEX_HOME`, the plugin uses
  `~/.gray/codex-home` — its login never rotates or disturbs the one your
  own `codex` CLI holds in `~/.codex`.
- **Lists models from the CLI.** `model/list` paged through the
  app-server; nothing is hardcoded or borrowed.
- **Reports quota from the CLI.** `account/rateLimits/read` — the same
  read Codex's own rate-limit view uses.

## Build & install

```sh
git clone https://github.com/vstaln/gray-codex-sub.git
cd gray-codex-sub
cargo build --release
```

The binary lands at `./target/release/codex-sub`. Install it on `PATH` as
`gray-codex-sub` and register it with gray:

```sh
cp target/release/codex-sub ~/.local/bin/gray-codex-sub
gray install plugin codex-sub
```

Run the install from an interactive terminal and approve the
`provider.credentials` capability.

## Connecting

Sign in with the CLI first (see Requirements), then run `/connect` in
gray, pick **Codex (ChatGPT sign-in via Codex CLI)**, and choose a model:

```sh
/model codex-sub/gpt-5.1-codex
gray --model codex-sub/gpt-5.1-codex -p "Review this codebase"
```

`/codex status` shows the CLI's account, plan, and quota windows.
`/codex tools …` manages the upstream tool allowlist.

## Usage limits

For ChatGPT Plus users, the five-hour usage limit is **shared across all
apps** where you use your ChatGPT plan — usage here counts against the
same total as every other app. Track it in [ChatGPT Settings →
Usage](https://chatgpt.com/settings/usage) or `/codex status`.

## License

MIT

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
