# Bunsin (분신)

Bunsin hosts persona AI agents. Each agent holds one person's work principles and
work knowledge, and acts for that person in Slack, Jira and Confluence.

Status: early development. See [docs/PLAN.md](docs/PLAN.md) and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (both in Korean).

## Build

```sh
cargo test --workspace
```

`rust-toolchain.toml` pins the Rust version. rustup installs it on first use.
