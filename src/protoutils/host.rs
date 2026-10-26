use core::{
	fmt,
	net::{IpAddr, SocketAddr},
	str::FromStr,
};

use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

/// A host and port pair.
///
/// This mirrors how SSH names tunnel endpoints (`direct-tcpip` carries a host name
/// rather than a resolved address), and it is used instead of [`SocketAddr`] on the
/// wire so that postcard never has to encode platform-specific address types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostPort {
	/// Host name or numeric address. Bracketed IPv6 literals are accepted.
	pub host: String,
	/// Port number.
	pub port: u16,
}

/// An error raised while parsing a `host:port` string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseHostPortError {
	/// The string does not contain a `host:port` separator.
	#[error("expected 'host:port', got '{input}'")]
	MissingSeparator {
		/// The value that could not be parsed.
		input: String,
	},
	/// The port component was not a valid `u16`.
	#[error("invalid port '{port}': {reason}")]
	InvalidPort {
		/// The port component that failed to parse.
		port: String,
		/// Why parsing failed.
		reason: String,
	},
	/// The host component was empty.
	#[error("host component is empty")]
	EmptyHost,
}

impl HostPort {
	/// Creates a new `HostPort`.
	pub fn new(host: impl Into<String>, port: u16) -> Self {
		Self {
			host: host.into(),
			port,
		}
	}

	/// The host with any IPv6 brackets removed.
	#[must_use]
	pub fn bare_host(&self) -> &str {
		self.host
			.strip_prefix('[')
			.and_then(|h| h.strip_suffix(']'))
			.unwrap_or(&self.host)
	}

	/// Resolves the host to every socket address it maps to.
	///
	/// # Errors
	/// The host cannot be resolved.
	pub async fn resolve(&self) -> Result<Vec<SocketAddr>> {
		let host = self.bare_host();
		let addrs = tokio::net::lookup_host((host, self.port)).await?;
		Ok(addrs.collect())
	}

	/// Resolves the host and returns the first address it maps to.
	///
	/// # Errors
	/// The host cannot be resolved, or it resolves to no addresses.
	pub async fn resolve_first(&self) -> Result<SocketAddr> {
		let addrs = self.resolve().await?;
		addrs
			.into_iter()
			.next()
			.ok_or_else(|| eyre!("no addresses found for {}:{}", self.host, self.port))
	}
}

impl From<SocketAddr> for HostPort {
	fn from(value: SocketAddr) -> Self {
		let host = match value.ip() {
			IpAddr::V4(v4) => v4.to_string(),
			IpAddr::V6(v6) => format!("[{v6}]"),
		};
		Self::new(host, value.port())
	}
}

impl fmt::Display for HostPort {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}:{}", self.host, self.port)
	}
}

impl FromStr for HostPort {
	type Err = ParseHostPortError;

	fn from_str(string: &str) -> Result<Self, Self::Err> {
		let Some((host, port)) = string.rsplit_once(':') else {
			return Err(ParseHostPortError::MissingSeparator {
				input: string.to_owned(),
			});
		};
		if host.is_empty() {
			return Err(ParseHostPortError::EmptyHost);
		}
		let port: u16 = port.parse().map_err(|err: core::num::ParseIntError| {
			ParseHostPortError::InvalidPort {
				port: port.to_owned(),
				reason: err.to_string(),
			}
		})?;
		Ok(Self::new(host, port))
	}
}

#[cfg(test)]
mod tests {
	use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

	use color_eyre::eyre::Result;

	use super::{HostPort, ParseHostPortError};

	#[test]
	fn parses_host_and_port() -> Result<()> {
		assert_eq!(
			"example.test:8080".parse::<HostPort>()?,
			HostPort::new("example.test", 8080)
		);
		assert_eq!("[::1]:22".parse::<HostPort>()?, HostPort::new("[::1]", 22));
		assert_eq!("[::1]:22".parse::<HostPort>()?.bare_host(), "::1");
		Ok(())
	}

	#[test]
	fn rejects_malformed_host_and_port() {
		assert!(matches!(
			"no-port".parse::<HostPort>(),
			Err(ParseHostPortError::MissingSeparator { .. })
		));
		assert!(matches!(
			"host:99999".parse::<HostPort>(),
			Err(ParseHostPortError::InvalidPort { .. })
		));
		assert!(matches!(
			":22".parse::<HostPort>(),
			Err(ParseHostPortError::EmptyHost)
		));
	}

	#[test]
	fn round_trips_through_wire_format_and_display() -> Result<()> {
		let target = HostPort::new("127.0.0.1", 9090);
		let decoded = postcard::from_bytes::<HostPort>(&postcard::to_allocvec(&target)?)?;
		assert_eq!(decoded, target);
		assert_eq!(target.to_string(), "127.0.0.1:9090");
		Ok(())
	}

	#[test]
	fn brackets_ipv6_addresses_when_built_from_socket_addr() {
		let addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1234, 0, 0));
		let host_port = HostPort::from(addr);
		assert_eq!(host_port.host, "[::1]");
		assert_eq!(host_port.port, 1234);
		assert_eq!(
			HostPort::from(SocketAddr::from((Ipv4Addr::LOCALHOST, 80))),
			HostPort::new(Ipv4Addr::LOCALHOST.to_string(), 80)
		);
	}
}
