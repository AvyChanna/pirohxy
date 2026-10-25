# Pirohxy

Pirohxy is a SSH replacement that uses iroh as its transport layer. This means that you can connect to remote machines without exposing any ports, or adding special firewall rules.

It carries the SSH-style channels you would reach for `ssh -L`, `ssh -R`, `ssh host cmd`, `ssh -t`, `ssh -N` and `sftp` for, all over the same iroh connection.

## Commands

| Command                                     | What it does                                                                                             |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `pirohxy egress`                            | Runs the server side, accepting every channel the auth directory allows                                  |
| `pirohxy socks5 <server>`                   | Exposes a local SOCKS5 proxy through the server (`ssh -D`)                                               |
| `pirohxy forward <server> <target>`         | Forwards a local port to a target the server connects to (`ssh -L`)                                      |
| `pirohxy r-forward <server> <bind> <local>` | Makes the server listen on a port and hands the connections back (`ssh -R`)                              |
| `pirohxy exec <server> <cmd...>`            | Runs a command on the server and mirrors its exit code                                                   |
| `pirohxy shell <server>`                    | Opens an interactive shell on a pseudo terminal, with raw mode, real window size and resize (`ssh -t`) |
| `pirohxy file <server> <action>`            | Lists, stats, downloads, uploads, mkdirs, removes and moves paths inside the server's file root (`sftp`) |
| `pirohxy init`                              | Creates this node's identity                                                                             |
| `pirohxy conf path\|identity`               | Prints the config directory or this node's ID                                                            |

`forward`, `r-forward` and `file` also accept the `forward`, `rforward` and `sftp` aliases.

## Getting Started

### Prerequisites

- Rust (Recommended: Install via [Rustup](https://rustup.rs/))

### Build

```sh
cargo build --release
```

### Run

```sh
cargo run --release
```

### Install

```sh
cargo install pirohxy
```

### Test

```sh
cargo nextest run --all-targets
```

Tests run with [cargo-nextest](https://nexte.st/) (configured by `.config/nextest.toml`). The
`cli_smoke` suite drives the real `pirohxy` binary against a real `pirohxy egress` over all
seven channels, so it is the end-to-end proof; `cargo nextest run --test cli_smoke` runs just
that suite.

## Configuration

Configs can be modified by simple file ops. No need to restart the app itself. See examples below.

The config directory holds:

| Path                     | Purpose                                                            |
| ------------------------ | ------------------------------------------------------------------ |
| `self.priv` / `self.pub` | This node's identity.                                              |
| `auth/`                  | Which remote identities may connect, and to which protocols.       |
| `names/`                 | Aliases for remote identities, so you do not have to remember IDs. |
| `files/`                 | Default directory the `file` protocol is confined to.              |

Run `pirohxy conf path` to print the directory.

#### Server

```sh
# Init identity
pirohxy init

# Print server ID
pirohxy conf identity

# Whitelist a client to allow connections
CONF_DIR="$(pirohxy conf path)"
CLIENT_ID="<Get identity from client>"

mkdir -p "${CONF_DIR}/auth/${CLIENT_ID}"
touch "${CONF_DIR}/auth/${CLIENT_ID}/socks"

# Start the proxy
pirohxy egress

# Start the proxy with the SSH-like subsystems open to whitelisted clients
pirohxy egress --file-root ~/pirohxy-files --shell /bin/bash
```

A grant is a file named after a protocol inside the client's directory. Open as many
protocols as you need:

```sh
mkdir -p "${CONF_DIR}/auth/${CLIENT_ID}"
touch "${CONF_DIR}/auth/${CLIENT_ID}/tunnel"     # local forward
touch "${CONF_DIR}/auth/${CLIENT_ID}/rtunnel"    # remote forward
touch "${CONF_DIR}/auth/${CLIENT_ID}/exec"       # run a command
touch "${CONF_DIR}/auth/${CLIENT_ID}/shell"      # interactive shell
touch "${CONF_DIR}/auth/${CLIENT_ID}/file"       # file transfer

# Or hand over everything at once
touch "${CONF_DIR}/auth/${CLIENT_ID}/all"
```

Grants are read per connection, so changes apply without a restart.

#### Client

```sh
# Init identity
pirohxy init

# Print client ID
pirohxy conf identity

# (optional) Register an alias for server
CONF_DIR="$(pirohxy conf path)"
SERVER_ID="<Get identity from server>"
SERVER_ALIAS="<alias name for server, so you won't have to remember IDs>"
echo ${SERVER_ID} > ${CONF_DIR}/names/${SERVER_ALIAS}

# SOCKS5 proxy, the equivalent of `ssh -D`
pirohxy socks5 ${SERVER_ALIAS} --bind 127.0.0.1:1080
# OR directly use server ID without an alias (by adding a colon as prefix)
pirohxy socks5 :${SERVER_ID}

# Local forward, the equivalent of `ssh -L`
pirohxy forward ${SERVER_ALIAS} db.internal:5432 --bind 127.0.0.1:15432

# Remote forward, the equivalent of `ssh -R`
pirohxy r-forward ${SERVER_ALIAS} 0.0.0.0:8000 127.0.0.1:8080

# Run a command, the equivalent of `ssh host cmd`
pirohxy exec ${SERVER_ALIAS} uname -a
# Options for pirohxy itself come before the server name
pirohxy exec --cwd /var/log --env FOO=bar ${SERVER_ALIAS} sh -c 'tail -n 5 syslog'

# Interactive shell, the equivalent of `ssh -t host`
pirohxy shell ${SERVER_ALIAS}
pirohxy shell ${SERVER_ALIAS} --cmd bash
pirohxy shell --cwd /var/log --env FOO=bar ${SERVER_ALIAS}

# Transfer files, the equivalent of `sftp`
pirohxy file ${SERVER_ALIAS} ls
pirohxy file ${SERVER_ALIAS} stat notes.txt
pirohxy file ${SERVER_ALIAS} mkdir inbox
pirohxy file ${SERVER_ALIAS} put ./notes.txt inbox/notes.txt
pirohxy file ${SERVER_ALIAS} get inbox/notes.txt ./notes.txt
pirohxy file ${SERVER_ALIAS} get inbox/notes.txt -      # to stdout
pirohxy file ${SERVER_ALIAS} mv inbox/notes.txt done.md
pirohxy file ${SERVER_ALIAS} rm inbox/notes.txt
```

`file` paths are resolved inside the server's file root and cannot escape it. `exec` starts the
program directly without a shell, so builtins, pipelines and redirection need `sh -c '...'` (or
`cmd /c ...` on Windows). `exec` exits with the exit code of the remote command.

### Terminal multiplexing

Pirohxy carries the pseudo terminal; [Zellij](https://zellij.dev) can own everything above it - panes,
tabs, scrollback, sessions that keep running after you disconnect. Nothing in this repository knows
Zellij exists; it is reached through the commands above. The shell client runs the local terminal in raw
mode, reports the real window size and forwards resize, so a full-screen program like Zellij lays out
correctly and resizes with your window. Ctrl-C is forwarded as the `0x03` keystroke byte rather than as
a signal that ends the session, so Zellij detaches (and a shell just abandons the line being typed)
instead of dying:

```sh
# A workspace on the server you can leave and come back to
pirohxy shell ${SERVER_ALIAS} --cmd "zellij attach work --create"

# Start work detached, read it later, end it when done
pirohxy exec ${SERVER_ALIAS} -- zellij attach work --create-background -- cargo watch
pirohxy exec ${SERVER_ALIAS} -- zellij list-sessions --short
pirohxy exec ${SERVER_ALIAS} -- zellij action dump-screen --pane-id terminal_1 --full
pirohxy exec ${SERVER_ALIAS} -- zellij kill-session work

# Zellij's own browser client, carried over iroh instead of an exposed port
pirohxy forward ${SERVER_ALIAS} 127.0.0.1:8082 --bind 127.0.0.1:8082
```

The `exec` lines need one thing more than they look like they do: `exec` clears the environment, and
Zellij wants a home, a temp directory and a config directory before it will run at all - `shell --cmd`
inherits the server's own and needs nothing. The variables to pass are listed in the guide.

[docs/zellij.md](docs/zellij.md) has the full guide: installation, the headless `exec` control,
layouts, session isolation, and the target Zellij-socket integration that is still to be built.


## Contributing

This is a hobby project, so expect some rough edges. For any feedback or feature suggestions, pull requests and issues are always welcome!
