# Piroxy

Piroxy is a SOCKS5 proxy that uses iroh as its transport layer. This means that you can connect to remote machines without exposing any ports, or adding special firewall rules.

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

## Configuration

Configs can be modified by simple file ops. No need to restart the app itself. See examples below. 

#### Server

```sh
# Init identity
pirohxy init

# Print server ID
pirohxy conf identity

# Whitelist a client to allow connections
CONF_DIR="$(pirohxy conf path)"
CLIENT_ID="<Get identity from client>"

touch "${CONF_DIR}/auth/${CLIENT_ID}"
# OR Optionally, write some server metadata as well. This is ignored by the app
echo "My dev server in XYZ VPS with IP 1.2.3.4" > "${CONF_DIR}/auth/${CLIENT_ID}"

# Start the proxy
pirohxy egress
```

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
echo ${SERVER_ID} > ${CONF_DIR}/${SERVER_ALIAS}

# Connect to the server
pirohxy connect ${SERVER_ALIAS}
# OR directly use server ID without an alias (by adding a colon as prefix)
pirohxy connect :${SERVER_ID}
```

## Contributing

This is a hobby project, so expect some rough edges. For any feedback or feature suggestions, pull requests and issues are always welcome!
