# gray-codex-sub

> **ChatGPT/Codex subscription model-provider sidecar plugin for the [gray](https://github.com/vstaln/gray) agent harness.**

`gray-codex-sub` lets gray run turns on your ChatGPT plan instead of an API key. It is a protocol-1.2 provider sidecar: gray owns the agent loop, tools, approvals, and compaction — the plugin only supplies the OAuth login and the provider declaration the host uses for each Responses request.

## Auth method

**ChatGPT login via Codex CLI client (unofficial)** — this plugin signs in with the Codex CLI's own OAuth client and sends requests to `https://chatgpt.com/backend-api/codex`, the same internal backend the Codex CLI uses. It works today, but it is unofficial: OpenAI can change or close that backend at any time, which would break this provider without warning.

> **Planned:** the official **Sign in with ChatGPT** login — OpenAI's supported flow for open-source apps, using ChatGPT plan usage on `https://api.openai.com/v1` — is coming in a later release.

## Build & install

```sh
git clone https://github.com/vstaln/gray-codex-sub.git
cd gray-codex-sub
cargo build --release
```

The binary lands at `./target/release/codex-sub`. Install it on `PATH` as `gray-codex-sub` and register it with gray:

```sh
cp target/release/codex-sub ~/.local/bin/gray-codex-sub
gray install plugin codex-sub
```

Run the install from an interactive terminal and approve the `provider.credentials` capability.

## Connecting

Once registered, run `/connect` in gray, pick **Codex backend (unofficial)**, and complete the browser sign-in. Then choose a model:

```sh
/model codex-sub/gpt-5.1-codex
gray --model codex-sub/gpt-5.1-codex -p "Review this codebase"
```

## Usage limits

For ChatGPT Plus users, the five-hour usage limit is **shared across all apps** where you use your ChatGPT plan — usage here counts against the same total as every other app. Track it in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

## License

MIT
