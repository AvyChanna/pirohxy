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

Edit the configuration files in the `config/` directory to customize authentication, name alais, and identity.

#### Server

```sh
# Init identity
pirohxy init

# Print server ID
pirohxy info

# Whitelist a client to allow connections
touch /path/to/config/auth/${client_id}

# Start the proxy
pirohxy egress
```

#### Client

```sh
# Init identity
pirohxy init

# Print client ID
pirohxy info

# Register an alias for server
echo ${server_id} > /path/to/config/${server_name}

# Connect to the server
pirohxy connect ${server_name}
# OR directly use server ID without an alias
pirohxy connect :${server_id}
```

## Contributing

This is a hobby project, so expect some rough edges. For any feedback or feature suggestions, pull requests and issues are always welcome!
