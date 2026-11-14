# Bunsin (분신)

Bunsin hosts persona AI agents. Each agent holds one person's work principles and
acts for that person in Slack, Jira and Confluence through MCP servers.
One installation holds several isolated agent profiles.

The LLM only proposes. The harness decides what runs:

- A permission table per profile lists the allowed tools. A tool outside the table is denied.
- Every write starts at "run after the owner approves". The owner raises the level per tool.
- The harness appends an AI marker to every text the agent sends to other people.
- A kill switch per profile stops all writes.
- Every proposal, draft and result is saved, and an append-only audit log records each step.

Status: early development. Read [docs/PLAN.md](docs/PLAN.md) for the plan and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the list of what
is implemented (both in Korean).

## Build and test

```sh
cargo test --workspace
```

`rust-toolchain.toml` pins the Rust version. rustup installs it on first use.
The window uses GPUI through the `gpui-kit` crate. Builds are tested on macOS only.

## Use

`bunsind` is the headless command line. `bunsin-app` is the desktop window.
Both use the folder named by `BUNSIN_DATA_DIR`, one process at a time.

```sh
export BUNSIN_DATA_DIR="$HOME/bunsin-data"
cargo run -p bunsin-daemon -- profile-add kim --owner "Kim"
cargo run -p bunsin-daemon -- config-show kim > kim.json
# Edit kim.json, then:
cargo run -p bunsin-daemon -- config-set kim kim.json
cargo run -p bunsin-daemon -- status kim
cargo run -p bunsin-app
```

A new profile can do nothing until its settings name an LLM endpoint, connectors,
approved tools and permission rules. Example of the `llm` and `connectors` settings:

```json
{
  "llm": {
    "wire": "anthropic-messages",
    "base_url": "https://llm.example.com",
    "model": "your-model-id",
    "token_env": "BUNSIN_LLM_TOKEN"
  },
  "connectors": {
    "slack": {
      "mcp": { "transport": "http", "url": "https://mcp.example.com/slack" },
      "token_env": "BUNSIN_SLACK_TOKEN"
    }
  }
}
```

`wire` is `anthropic-messages` or `openai-chat`. Settings hold the name of an
environment variable, never a token. Set those variables before you start
`bunsind` or `bunsin-app`.
