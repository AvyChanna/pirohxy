use core::fmt;

/// ALPN identifier for the SOCKS5 proxy channel.
pub const ALPN_SOCKS: &[u8] = b"/avychanna/pirohxy/socks/v1";
/// ALPN identifier for the local forward (`direct-tcpip`) channel.
pub const ALPN_TUNNEL: &[u8] = b"/avychanna/pirohxy/tunnel/v1";
/// ALPN identifier for the remote forward (`forwarded-tcpip`) channel.
pub const ALPN_REVERSE_TUNNEL: &[u8] = b"/avychanna/pirohxy/rtunnel/v1";
/// ALPN identifier for the command execution channel.
pub const ALPN_EXEC: &[u8] = b"/avychanna/pirohxy/exec/v1";
/// ALPN identifier for the interactive shell (`pty-req` + `shell`) channel.
pub const ALPN_SHELL: &[u8] = b"/avychanna/pirohxy/shell/v1";
/// ALPN identifier for the file transfer subsystem.
pub const ALPN_FILE: &[u8] = b"/avychanna/pirohxy/file/v1";

/// The list of protocol names accepted by [`Protocol`], for error messages.
pub const KNOWN_PROTOCOL_NAMES: &str = "socks, tunnel, rtunnel, exec, shell, file (aliases: socks5, forward, reverse-tunnel, tty, sftp)";

/// An error raised while parsing a protocol name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseProtocolError {
	/// The given name does not match any known protocol.
	#[error("unknown protocol '{input}' (expected one of: {KNOWN_PROTOCOL_NAMES})")]
	Unknown {
		/// The name that could not be parsed.
		input: String,
	},
}

/// Every SSH-style channel this crate exposes, each negotiated on its own ALPN.
///
/// Convention for the identifiers is `namespace/app/proto/version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
	/// SOCKS5 proxy, the equivalent of `ssh -D`.
	Socks,
	/// Local forward, the equivalent of `ssh -L` (`direct-tcpip`).
	Tunnel,
	/// Remote forward, the equivalent of `ssh -R` (`forwarded-tcpip`).
	ReverseTunnel,
	/// Non-interactive command execution, the equivalent of `ssh host <cmd>`.
	Exec,
	/// Interactive shell on a pseudo terminal, the equivalent of `ssh -t host`.
	Shell,
	/// File transfer, the equivalent of the `sftp` subsystem.
	File,
}

impl Protocol {
	/// Every protocol, in registration order.
	pub const ALL: [Self; 6] = [
		Self::Socks,
		Self::Tunnel,
		Self::ReverseTunnel,
		Self::Exec,
		Self::Shell,
		Self::File,
	];

	/// The ALPN bytes this protocol is registered under.
	#[must_use]
	pub const fn alpn(self) -> &'static [u8] {
		match self {
			Self::Socks => ALPN_SOCKS,
			Self::Tunnel => ALPN_TUNNEL,
			Self::ReverseTunnel => ALPN_REVERSE_TUNNEL,
			Self::Exec => ALPN_EXEC,
			Self::Shell => ALPN_SHELL,
			Self::File => ALPN_FILE,
		}
	}

	/// The short name used by the CLI and by logs.
	#[must_use]
	pub const fn name(self) -> &'static str {
		match self {
			Self::Socks => "socks",
			Self::Tunnel => "tunnel",
			Self::ReverseTunnel => "rtunnel",
			Self::Exec => "exec",
			Self::Shell => "shell",
			Self::File => "file",
		}
	}
}

impl fmt::Display for Protocol {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.name())
	}
}

impl core::str::FromStr for Protocol {
	type Err = ParseProtocolError;

	fn from_str(string: &str) -> Result<Self, Self::Err> {
		match string.to_ascii_lowercase().as_str() {
			"socks" | "socks5" | "socks5-server" => Ok(Self::Socks),
			"tunnel" | "forward" | "direct-tcpip" => Ok(Self::Tunnel),
			"rtunnel" | "reverse-tunnel" | "rforward" | "forwarded-tcpip" => {
				Ok(Self::ReverseTunnel)
			}
			"exec" | "command" => Ok(Self::Exec),
			"shell" | "tty" | "pty" => Ok(Self::Shell),
			"file" | "sftp" => Ok(Self::File),
			_ => Err(ParseProtocolError::Unknown {
				input: string.to_owned(),
			}),
		}
	}
}

#[cfg(test)]
mod tests;
