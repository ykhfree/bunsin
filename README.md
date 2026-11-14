# Bunsin (분신)

[한국어](README.ko.md)

Bunsin hosts persona AI agents. Each agent holds one person's work principles and
acts for that person in Slack, Jira and Confluence through MCP servers.
One server holds several isolated agent profiles.

The LLM only proposes. The harness decides what runs:

- A permission table per profile lists the allowed tools. A tool outside the table is denied.
- Every write starts at "run after the owner approves". The owner raises the level per tool.
- The harness appends an AI marker to every text the agent sends to other people.
- A kill switch per profile stops all writes.
- Every proposal, draft and result is saved, and an append-only audit log records each step.

## Current limits

Bunsin is in early development. Read this list before you use it.

- **Finding events depends on your MCP servers.** The server repeats a search tool that
  you name in the settings (see [Find events](#find-events-polling)). It has run only
  against test servers. There is no webhook intake.
- **One event gives one proposal.** The agent does not yet read a tool result and continue.
- **The privacy filter works on patterns.** It finds email addresses, phone numbers and
  the other listed forms. It does not find names or postal addresses.
- **There is no setup wizard.** You edit the settings as a JSON file.
- **Not built yet:** retrieval of work knowledge, ARD publishing and search, the REST API
  connector, OAuth for MCP servers, the scoring of replay evaluation.
- **Tested only against local test servers.** No test runs against a real LLM endpoint or
  a real MCP server.
- **The desktop window is tested headlessly.** The Keychain storage has no test.
- **The server operator can read profile data.** Profiles are isolated from each other,
  not from a person with access to the server's files.

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) has the full list of what is implemented,
and [docs/PLAN.md](docs/PLAN.md) has the plan (both in Korean).

## Parts

| Part | What it is | Runs on |
|---|---|---|
| `bunsind serve` | The server. It runs the agents and stores their data. | Linux or macOS |
| `bunsind <command>` | The command line of an operator or a profile owner. | Linux or macOS |
| `bunsin-app` | The desktop window: approval inbox, audit log, kill switch. | macOS |

## Build and test

```sh
cargo build --release -p bunsin-daemon     # the server and command line: target/release/bunsind
cargo build --release -p bunsin-app        # the desktop window (macOS): target/release/bunsin-app
cargo test --workspace
```

`rust-toolchain.toml` pins the Rust version. rustup installs it on first use.
On Linux, build and test without the window: `cargo test --workspace --exclude bunsin-app`.
The window uses GPUI through the `gpui-kit` crate.

## Set up the server (operator)

```sh
export BUNSIN_DATA_DIR=/var/lib/bunsin
bunsind profile-add kim --owner "Kim"
bunsind token-add kim --label "Kim's laptop"   # prints the access token once
export BUNSIN_SECRET_KIM__LLM=...               # the LLM endpoint token of profile kim
export BUNSIN_SECRET_KIM__SLACK=...             # the Slack MCP token of profile kim
bunsind serve --listen 0.0.0.0:8787 --tls-cert cert.pem --tls-key key.pem
```

- **Access tokens.** A token gives access to one profile. Only its hash is stored, so it
  cannot be shown again. `bunsind token-list` and `bunsind token-revoke <id>` manage tokens.
  Profiles and tokens are created on the server only, never over the network.
- **TLS.** The server refuses to listen on a public address without TLS. Behind a TLS
  proxy, listen on `127.0.0.1`. To try Bunsin on one machine, run
  `bunsind serve --listen 127.0.0.1:8787` and use `http://127.0.0.1:8787` as the address.
- **Secrets.** Profile settings name a secret key such as `LLM`. The server reads the value
  from the environment variable `BUNSIN_SECRET_<PROFILE>__<KEY>`. A profile cannot name
  another profile's secret.
- **Upstream addresses.** Profile owners choose the LLM and MCP addresses, and the server
  sends the requests. By default the server accepts any `https://` address except
  `localhost` and internal IP addresses. A host name that resolves to an internal address
  still passes. On a shared or public server, start it with `--allow-upstream-host <host>`
  (repeatable): then only the listed hosts are allowed, including an internal gateway
  over `http://`.
- **Local MCP commands.** MCP servers that run as a command on the server need
  `--allow-stdio-connectors`.
- **Stopping.** On SIGTERM the server takes no new request and waits up to 300 seconds
  for running ones.

## Set up an agent (profile owner)

```sh
export BUNSIN_SERVER=https://bunsin.example.com:8787
export BUNSIN_TOKEN=bsn_...                    # from the operator
bunsind config-show kim > kim.json
# Edit kim.json, then:
bunsind config-set kim kim.json
```

For a server with a private CA, set `BUNSIN_CA_CERT` to the CA certificate file.

A new profile can do nothing. Its settings must name an LLM endpoint, connectors,
approved tools and permission rules.

### Principles, LLM and connectors

```json
{
  "principles": [
    "Answer in two sentences.",
    "Never promise a date without checking Jira."
  ],
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

- `principles` go into every prompt as instructions. All other text the agent reads
  (messages, pages, tool descriptions) is quoted as data.
- `wire` is `anthropic-messages` or `openai-chat`.
- `token_secret` is a secret key. With profile `kim`, `"LLM"` means the server's
  environment variable `BUNSIN_SECRET_KIM__LLM`.
- A connector with `"transport": "stdio"` takes `command` (an absolute path), `args`
  and `env` instead of `url`.

### Permission rules

```json
{
  "approved_tools": {
    "slack": [
      {
        "name": "search_messages",
        "description": "Search messages",
        "input_schema": { "type": "object", "properties": { "query": { "type": "string" } } }
      },
      {
        "name": "post_message",
        "description": "Post a message",
        "input_schema": {
          "type": "object",
          "properties": { "channel": { "type": "string" }, "text": { "type": "string" } }
        }
      }
    ]
  },
  "policy": {
    "rules": {
      "slack/search_messages": { "class": "read", "tier": "autonomous", "outgoing_text": "none" },
      "slack/post_message": { "class": "write", "tier": "approve", "outgoing_text": { "arg": "text" } }
    }
  }
}
```

- **`approved_tools`** holds each tool exactly as the MCP server lists it: name,
  description and input schema. You do not type it. `bunsind tools kim slack` shows the
  tools of connector `slack` and marks each as `new`, `changed`, `approved` or `gone`;
  `bunsind tools kim slack --approve` approves exactly the list it showed. Before every
  call the server compares the approved list with the live one. If a tool was changed or
  removed, the call is denied until you approve the list again. Listing tools needs the
  `llm` and `connectors` settings to be in place.
- **`policy.rules`** is the permission table. A key is `<connector>/<tool>`. The agent can
  propose only tools that have a rule and an approved entry.
- **`class`** is `read`, `write` or `delete`. A read runs at once, also when the kill
  switch is on.
- **`tier`** says how far a write may go:

  | Tier | What happens to a proposed write |
  |---|---|
  | `observe` | The proposal is saved. Nothing is shown or run. |
  | `draft` | The draft appears in the inbox. The agent never runs it. |
  | `approve` | The call runs after you approve it. Start here. |
  | `autonomous` | The call runs without asking. A `delete` still needs approval. |

- **`outgoing_text`** is required in every rule. `{ "arg": "text" }` names the argument
  that other people will read; the server appends `ai_marker` to it. `"none"` means the
  tool sends no text to other people. The marker covers one string argument only.

### Find events (polling)

```json
{
  "polls": [
    {
      "connector": "slack",
      "tool": "search_messages",
      "args": { "query": "to:me" },
      "every_secs": 60,
      "mapping": { "items": "messages", "id": "ts", "text": "text", "author": "user", "thread": "thread_ts" },
      "skip_authors": ["U012AB3CD"]
    }
  ]
}
```

The server calls the tool every `every_secs` seconds (30 at least) and hands each new
item to the agent.

- The tool must have a rule with the class `read` and must be in the approved list.
- `mapping` says where the fields are in the tool's result, as dot paths such as
  `user.id`. `items` is the path to the list; leave it empty if the result is the list.
  The ID must stay the same for the same message. `thread` may be `null`.
- `bunsind poll-test kim 0` runs poll number 0 once and prints the events it would hand
  over. Use it to get the mapping right. It stores nothing.
- The first run of a search records what it finds as already seen. Only later items
  become events. A changed search (`connector`, `tool` or `args`) starts again that way.
- `skip_authors` lists your own account names or IDs. Your messages and texts that carry
  the AI marker are not events.
- An event whose handling fails is tried again on a later run, three times in all. Then
  an item in the inbox tells you.
- Polling pauses while the kill switch is on.
- `bunsind status kim` shows the last result of each poll. "none usable" means the
  mapping does not fit the result.

### Privacy filter

```json
{
  "privacy": {
    "email": true,
    "phone": true,
    "resident_number": true,
    "card_number": true,
    "custom": [{ "label": "employee id", "pattern": "\\bEMP-\\d{6}\\b" }]
  }
}
```

Before the server stores an event or sends any quoted text to the LLM, it replaces each
match with a placeholder such as `[REDACTED:EMAIL]`. The removed text is in no database,
file or log; the audit log holds counts only.

- The four built-in kinds are on in a new profile. `resident_number` is the Korean
  resident registration number. A card number counts only if its check digit is right.
- `custom` patterns use the syntax of the Rust [`regex`](https://docs.rs/regex) crate.
  The server refuses a pattern that does not compile or that matches empty text.
- The desktop window has a form for these settings.
- Your own `principles` and your own edits are not filtered.

### Work with the agent

```sh
bunsind ingest kim --source slack --event-id 1700000000.000100 \
    --author lee --text "When do we deploy?"
bunsind status kim                 # the kill switch, the polls and the inbox
bunsind approve kim 1              # run pending item 1
bunsind approve kim 1 --args '{"channel":"C1","text":"At 3 pm."}' --reason "shorter"
bunsind reject kim 1 --reason "not needed"
bunsind stop kim on                # the kill switch: stops all writes of this profile
```

An edit with a reason is stored as a correction. The agent sees recent corrections in
its next prompts.

## Desktop window (macOS)

```sh
cargo run --release -p bunsin-app
```

On first start the window asks for the server address, the access token and, for a
private CA, the CA certificate file. It checks them against the server, then keeps the
address in `~/Library/Application Support/Bunsin/connection.json` and the token in the
macOS Keychain.

The window shows the profile, the approval inbox with every argument of each call, the
audit log, the kill switch and the privacy settings. It reloads from the server every 5 seconds. An edit before
approval is available on the command line only.

For development, `BUNSIN_SERVER` and `BUNSIN_TOKEN` in the environment take the place of
the saved connection.

## License

Apache-2.0. See [LICENSE](LICENSE).
