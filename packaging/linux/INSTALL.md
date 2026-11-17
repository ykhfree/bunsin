# Install the Bunsin server on Linux (systemd)

This package holds `bunsind`, a systemd unit, a sysusers file and an environment template.
Nobody has run this package on a real Linux server yet. CI builds it on Ubuntu; the unit
has had no start under systemd.

## Files

| File in the package | Install as | Owner, mode |
|---|---|---|
| `bunsind` | `/usr/local/bin/bunsind` | root, 0755 |
| `bunsind.service` | `/etc/systemd/system/bunsind.service` | root, 0644 |
| `bunsin.sysusers` | `/usr/lib/sysusers.d/bunsin.conf` | root, 0644 |
| `bunsind.env.example` | `/etc/bunsin/bunsind.env` | root:bunsin, 0640 |
| `INSTALL.md` | `/usr/local/share/doc/bunsin/INSTALL.md` | root, 0644 |

## Steps

Run each step as root.

1. Unpack the tarball and go into its directory.
2. Install the files:

   ```sh
   install -m 0755 bunsind /usr/local/bin/bunsind
   install -m 0644 bunsind.service /etc/systemd/system/bunsind.service
   install -m 0644 bunsin.sysusers /usr/lib/sysusers.d/bunsin.conf
   install -d -m 0755 /usr/local/share/doc/bunsin
   install -m 0644 INSTALL.md /usr/local/share/doc/bunsin/INSTALL.md
   ```

3. Create the user `bunsin`: `systemd-sysusers`.
4. Install the environment file: `install -d -m 0750 -g bunsin /etc/bunsin`, then
   `install -m 0640 -g bunsin bunsind.env.example /etc/bunsin/bunsind.env`.
5. Edit `/etc/bunsin/bunsind.env`: set `BUNSIND_SERVE_ARGS` (listeners, TLS files, allowed hosts).
6. Start the server once, so that it creates its directories and its key file:
   `systemctl daemon-reload && systemctl enable --now bunsind`.
7. Create a profile and an access token. Operator commands read the data directory directly,
   so run them as the user `bunsin`, with the same environment:

   ```sh
   runuser -u bunsin -- env BUNSIN_DATA_DIR=/var/lib/bunsin/data bunsind profile-add kim --owner "Kim"
   runuser -u bunsin -- env BUNSIN_DATA_DIR=/var/lib/bunsin/data bunsind token-add kim --label "Kim's laptop"
   ```

8. Copy `/var/lib/bunsin/key/master.key` to a safe place outside the backups of `/var/lib/bunsin/data`.
   Without it, the stored tokens cannot be read again.

`journalctl -u bunsind` shows the log. `systemctl stop bunsind` waits until the running requests
end, up to 300 seconds: the unit sets `TimeoutStopSec=330`. A request that is cut leaves its
item `unknown`; the owner resolves it.

## The hardening of the unit, and what to change

The unit runs as `bunsin` with `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=yes`,
`PrivateTmp`, `PrivateDevices`, no capabilities, and only the IPv4, IPv6 and Unix socket families.
The whole file system is read-only for the server, except `/var/lib/bunsin` (its home,
the data directory and the key directory) and its private `/tmp`.

- **`--allow-stdio-connectors`.** The server starts the programs of command connectors as the user
  `bunsin`, with the same limits. A program that writes outside `/var/lib/bunsin` and `/tmp` needs
  `ReadWritePaths=<path>` in a drop-in file (`systemctl edit bunsind`). Programs that `npx` or `uvx`
  start keep their caches in the home, `/var/lib/bunsin`, which is writable.
- **`--allow-coding`.** A coding job adds a git worktree, which writes into the `.git` directory
  of the repository. Add `ReadWritePaths=<the --coding-repo-root>` in a drop-in file. The coding CLI
  keeps its sign-in and settings in the home of `bunsin`. Sign it in as that user:
  `runuser -u bunsin -- env HOME=/var/lib/bunsin claude` (or `codex`).
- **A port below 1024**, for example 443, needs `AmbientCapabilities=CAP_NET_BIND_SERVICE` and
  `CapabilityBoundingSet=CAP_NET_BIND_SERVICE` in a drop-in file. A reverse proxy in front of a
  high port needs nothing.
- **Left out on purpose:**
  - `MemoryDenyWriteExecute=yes` stops programs with a JIT compiler, for example Node.js, which
    many MCP servers and coding CLIs use.
  - `RestrictNamespaces=yes` stops the sandbox of a coding CLI that uses Linux namespaces.
  - `SystemCallFilter=` is not set, because the programs of stdio connectors and coding CLIs are not known.
  Without `--allow-stdio-connectors` and `--allow-coding`, you can add all three in a drop-in file.
- **Secrets.** The environment file is the only place for secret values (`BUNSIN_SECRET_...`).
  Tokens that owners enter are stored encrypted with the key file. TLS key files must be readable
  by `bunsin` and by nobody else.
