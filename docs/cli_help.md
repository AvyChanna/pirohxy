# Command-Line Help for `pirohxy`

This document contains the help content for the `pirohxy` command-line program.

**Command Overview:**

* [`pirohxy`↴](#pirohxy)
* [`pirohxy egress`↴](#pirohxy-egress)
* [`pirohxy socks5`↴](#pirohxy-socks5)
* [`pirohxy tunnel`↴](#pirohxy-tunnel)
* [`pirohxy r-forward`↴](#pirohxy-r-forward)
* [`pirohxy exec`↴](#pirohxy-exec)
* [`pirohxy shell`↴](#pirohxy-shell)
* [`pirohxy file`↴](#pirohxy-file)
* [`pirohxy file ls`↴](#pirohxy-file-ls)
* [`pirohxy file stat`↴](#pirohxy-file-stat)
* [`pirohxy file get`↴](#pirohxy-file-get)
* [`pirohxy file put`↴](#pirohxy-file-put)
* [`pirohxy file mkdir`↴](#pirohxy-file-mkdir)
* [`pirohxy file rm`↴](#pirohxy-file-rm)
* [`pirohxy file mv`↴](#pirohxy-file-mv)
* [`pirohxy init`↴](#pirohxy-init)
* [`pirohxy conf`↴](#pirohxy-conf)
* [`pirohxy conf path`↴](#pirohxy-conf-path)
* [`pirohxy conf identity`↴](#pirohxy-conf-identity)

## `pirohxy`

SOCKS proxy with Iroh transport

**Usage:** `pirohxy [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `egress` — Run egress
* `socks5` — Connect to an egress server
* `tunnel` — Forward a local port to a target through the server
* `r-forward` — Make the server listen on a port and hand the connections back
* `exec` — Run a command on the server
* `shell` — Open an interactive shell on the server
* `file` — Transfer files with the server
* `init` — Initialize an identity
* `conf` — Print current configuration

###### **Options:**

* `-c`, `--config <CONFIG-DIR>`



## `pirohxy egress`

Run egress

**Usage:** `pirohxy egress [OPTIONS]`

###### **Options:**

* `--server-req-timeout <SERVER_REQ_TIMEOUT>`

  Default value: `5s`
* `--file-root <DIR>` — Directory the `file` protocol is confined to
* `--shell <PROGRAM>` — Program to run for a shell session that asks for no command



## `pirohxy socks5`

Connect to an egress server

**Usage:** `pirohxy socks5 [OPTIONS] <SERVER>`

###### **Arguments:**

* `<SERVER>` — server name, use `:node_id` to connect directly

###### **Options:**

* `-b`, `--bind <BIND_ADDR>`

  Default value: `127.0.0.1:8080`
* `--conn-timeout <CONN_TIMEOUT>`

  Default value: `10s`



## `pirohxy tunnel`

Forward a local port to a target through the server

**Usage:** `pirohxy tunnel [OPTIONS] <SERVER> <TARGET>`

###### **Arguments:**

* `<SERVER>` — server name, use `:node_id` to connect directly
* `<TARGET>` — Target the server should connect to, as `host:port`

###### **Options:**

* `-b`, `--bind <BIND>` — Local address to listen on

  Default value: `127.0.0.1:8080`



## `pirohxy r-forward`

Make the server listen on a port and hand the connections back

**Usage:** `pirohxy r-forward <SERVER> <BIND> <LOCAL>`

###### **Arguments:**

* `<SERVER>` — server name, use `:node_id` to connect directly
* `<BIND>` — Address the server should listen on, as `host:port`
* `<LOCAL>` — Local address to bridge those connections to, as `host:port`



## `pirohxy exec`

Run a command on the server

**Usage:** `pirohxy exec [OPTIONS] <SERVER> [COMMAND]...`

###### **Arguments:**

* `<SERVER>` — server name, use `:node_id` to connect directly
* `<COMMAND>` — Program and its arguments. Everything from here on is handed to the server, flags included, so this side's own options have to come before the server name

###### **Options:**

* `--cwd <DIR>` — Working directory on the server
* `--env <KEY=VALUE>` — Variable to run the command with, repeatable



## `pirohxy shell`

Open an interactive shell on the server

**Usage:** `pirohxy shell [OPTIONS] <SERVER>`

###### **Arguments:**

* `<SERVER>` — server name, use `:node_id` to connect directly

###### **Options:**

* `--cmd <CMD>` — Program to run instead of the server's default shell
* `--term <TERM>`

  Default value: `xterm-256color`
* `--cwd <DIR>` — Working directory on the server
* `--env <KEY=VALUE>` — Variable to run the shell with, repeatable



## `pirohxy file`

Transfer files with the server

**Usage:** `pirohxy file <SERVER> <COMMAND>`

###### **Subcommands:**

* `ls` — List a directory
* `stat` — Report the metadata of one path
* `get` — Download a remote file
* `put` — Upload a local file
* `mkdir` — Create a directory and its parents
* `rm` — Remove a file or an empty directory
* `mv` — Move a path

###### **Arguments:**

* `<SERVER>` — server name, use `:node_id` to connect directly



## `pirohxy file ls`

List a directory

**Usage:** `pirohxy file ls [PATH]`

###### **Arguments:**

* `<PATH>`

  Default value: `.`



## `pirohxy file stat`

Report the metadata of one path

**Usage:** `pirohxy file stat <PATH>`

###### **Arguments:**

* `<PATH>`



## `pirohxy file get`

Download a remote file

**Usage:** `pirohxy file get <REMOTE> <LOCAL>`

###### **Arguments:**

* `<REMOTE>`
* `<LOCAL>` — local file to write, '-' for stdout



## `pirohxy file put`

Upload a local file

**Usage:** `pirohxy file put [OPTIONS] <LOCAL> <REMOTE>`

###### **Arguments:**

* `<LOCAL>`
* `<REMOTE>`

###### **Options:**

* `--append`



## `pirohxy file mkdir`

Create a directory and its parents

**Usage:** `pirohxy file mkdir <PATH>`

###### **Arguments:**

* `<PATH>`



## `pirohxy file rm`

Remove a file or an empty directory

**Usage:** `pirohxy file rm <PATH>`

###### **Arguments:**

* `<PATH>`



## `pirohxy file mv`

Move a path

**Usage:** `pirohxy file mv <FROM> <TO>`

###### **Arguments:**

* `<FROM>`
* `<TO>`



## `pirohxy init`

Initialize an identity

**Usage:** `pirohxy init`



## `pirohxy conf`

Print current configuration

**Usage:** `pirohxy conf <COMMAND>`

###### **Subcommands:**

* `path` — 
* `identity` — 



## `pirohxy conf path`

**Usage:** `pirohxy conf path`



## `pirohxy conf identity`

**Usage:** `pirohxy conf identity [IDENTITY_NAME]`

###### **Arguments:**

* `<IDENTITY_NAME>` — identity name



