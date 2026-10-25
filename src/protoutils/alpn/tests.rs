use color_eyre::eyre::Result;

use super::{KNOWN_PROTOCOL_NAMES, ParseProtocolError, Protocol};

#[test]
fn every_alpn_follows_the_namespace_convention_and_is_unique() -> Result<()> {
	let mut seen = Vec::new();
	for proto in Protocol::ALL {
		let alpn = proto.alpn();
		let alpn_str = core::str::from_utf8(alpn)?;
		assert!(
			alpn_str.starts_with("/avychanna/pirohxy/") && alpn_str.ends_with("/v1"),
			"unexpected ALPN for {proto}: {alpn_str}"
		);
		assert!(
			!seen.contains(&alpn),
			"duplicate ALPN for {proto}: {alpn_str}"
		);
		seen.push(alpn);
	}
	Ok(())
}

#[test]
fn parses_primary_names_and_aliases() -> Result<()> {
	let cases = [
		("socks", Protocol::Socks),
		("socks5", Protocol::Socks),
		("SOCKS5", Protocol::Socks),
		("tunnel", Protocol::Tunnel),
		("forward", Protocol::Tunnel),
		("direct-tcpip", Protocol::Tunnel),
		("rtunnel", Protocol::ReverseTunnel),
		("forwarded-tcpip", Protocol::ReverseTunnel),
		("exec", Protocol::Exec),
		("shell", Protocol::Shell),
		("tty", Protocol::Shell),
		("file", Protocol::File),
		("sftp", Protocol::File),
	];
	for (input, expected) in cases {
		assert_eq!(input.parse::<Protocol>()?, expected);
	}
	Ok(())
}

#[test]
fn rejects_unknown_protocol_names() {
	let err = "nope".parse::<Protocol>().expect_err("must reject");
	assert!(matches!(err, ParseProtocolError::Unknown { .. }));
	assert!(err.to_string().contains(KNOWN_PROTOCOL_NAMES));
}

#[test]
fn display_matches_canonical_name() {
	for proto in Protocol::ALL {
		assert_eq!(proto.to_string(), proto.name());
	}
}
