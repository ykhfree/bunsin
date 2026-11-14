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

`bunsind serve` runs the agents on a server. The desktop window (`bunsin-app`) and
the other `bunsind` commands manage them over HTTPS. To try it on one machine,
run the server on `127.0.0.1`.

### On the server (operator)

```sh
export BUNSIN_DATA_DIR=/var/lib/bunsin
bunsind profile-add kim --owner "Kim"
bunsind token-add kim --label "Kim's laptop"   # prints the access token once
export BUNSIN_SECRET_KIM__LLM=...               # the LLM endpoint token of profile kim
bunsind serve --listen 0.0.0.0:8787 --tls-cert cert.pem --tls-key key.pem
```

- An access token gives access to one profile. `token-list` and `token-revoke` manage tokens.
- The server refuses to listen on a public address without TLS. Behind a TLS proxy,
  listen on `127.0.0.1`.
- Profile settings name a secret key such as `LLM`. The server reads the value from
  `BUNSIN_SECRET_<PROFILE>__<KEY>`. A profile cannot name another profile's secret.
- Profile owners choose the LLM and MCP addresses, and the server sends the requests.
  By default the server accepts any `https://` address except `localhost` and internal
  IP addresses. A host name that resolves to an internal address still passes.
  On a shared or public server, start it with `--allow-upstream-host <host>` (repeatable):
  then only the listed hosts are allowed, including an internal gateway over `http://`.
- MCP servers that run as a local command need `--allow-stdio-connectors`.

### From your machine (profile owner)

```sh
export BUNSIN_SERVER=https://bunsin.example.com:8787
export BUNSIN_TOKEN=bsn_...
bunsind config-show kim > kim.json
# Edit kim.json, then:
bunsind config-set kim kim.json
bunsind status kim
bunsind stop kim on          # the kill switch
```

A new profile can do nothing until its settings name an LLM endpoint, connectors,
approved tools and permission rules. Example of the `llm` and `connectors` settings:

```json
{
  "llm": {
    "wire": "anthropic-messages",
    "base_url": "https://llm.example.com",
    "model": "your-model-id",
    "token_secret": "LLM"
  },
  "connectors": {
    "slack": {
      "mcp": { "transport": "http", "url": "https://mcp.example.com/slack" },
      "token_secret": "SLACK"
    }
  }
}
```

The desktop window asks for the server address and the access token on first start.
It keeps the token in the macOS Keychain.

`wire` is `anthropic-messages` or `openai-chat`. For a server with a private CA,
set `BUNSIN_CA_CERT` to the CA certificate file.
