use core::time::Duration;

use color_eyre::eyre::{Result, eyre};

use super::{Shell, ShellOutput, ShellRequest};
use crate::protocol::client::shell;
use crate::protoutils::{alpn::ALPN_SHELL, interproto::testnet};

/// Waits for a step, failing the test instead of hanging.
async fn within<T>(step: impl Future<Output = T>) -> T {
	tokio::time::timeout(Duration::from_secs(20), step)
		.await
		.expect("shell step timed out")
}

fn request(cmd: Option<String>) -> ShellRequest {
	ShellRequest {
		cmd,
		term: "xterm-256color".to_owned(),
		cols: 80,
		rows: 24,
		cwd: None,
		env: Vec::new(),
	}
}

#[tokio::test]
async fn client_shell_streams_the_output_of_a_requested_command() -> Result<()> {
	testnet::init_tracing();
	let server = testnet::spawn_server_on(ALPN_SHELL, Shell::new()).await?;
	let mut session = within(shell::open(
		server.conn(),
		&request(Some("echo shell-output-marker".to_owned())),
	))
	.await?;

	let mut transcript = Vec::new();
	let mut found = false;
	for _ in 0..20 {
		let Some(output) = within(session.next_output()).await else {
			break;
		};
		match output {
			ShellOutput::Data(chunk) => transcript.extend_from_slice(&chunk),
			ShellOutput::Exit { .. } => {}
			ShellOutput::Error(reason) => return Err(eyre!("shell failed: {reason}")),
		}
		if String::from_utf8_lossy(&transcript).contains("shell-output-marker") {
			found = true;
			break;
		}
	}
	assert!(
		found,
		"no shell output in {:?}",
		String::from_utf8_lossy(&transcript)
	);

	session.close().await?;
	Ok(())
}

#[tokio::test]
async fn client_shell_runs_a_session_from_keystrokes() -> Result<()> {
	testnet::init_tracing();
	let server = testnet::spawn_server_on(ALPN_SHELL, Shell::new()).await?;
	let mut session = within(shell::open(server.conn(), &request(None))).await?;

	session.resize(100, 30).await?;
	// The typed text itself never contains `line1`, so seeing it proves the shell ran
	// what the client sent.
	let keystrokes: &[u8] = if cfg!(windows) {
		b"for /l %i in (1,1,3) do echo line%i\r"
	} else {
		b"for i in 1 2 3; do echo line$i; done\r"
	};
	session.write_keystrokes(keystrokes).await?;

	let mut transcript = Vec::new();
	let mut found = false;
	for _ in 0..30 {
		let Some(output) = within(session.next_output()).await else {
			break;
		};
		match output {
			ShellOutput::Data(chunk) => transcript.extend_from_slice(&chunk),
			ShellOutput::Exit { .. } => {}
			ShellOutput::Error(reason) => return Err(eyre!("shell failed: {reason}")),
		}
		if String::from_utf8_lossy(&transcript).contains("line1") {
			found = true;
			break;
		}
	}
	assert!(
		found,
		"keystrokes did not run, transcript {:?}",
		String::from_utf8_lossy(&transcript)
	);

	session.close().await?;
	Ok(())
}

// Unix shell semantics: Ctrl-C abandons the line being typed and the shell keeps
// running. On Windows, `cmd.exe` at an empty prompt quits on Ctrl-C, which is the
// child's own choice rather than the server ending the session, so the assertion only
// holds on Unix.
#[cfg(unix)]
#[tokio::test]
async fn an_interrupt_keystroke_does_not_end_the_session() -> Result<()> {
	testnet::init_tracing();
	let server = testnet::spawn_server_on(ALPN_SHELL, Shell::new()).await?;
	let mut session = within(shell::open(server.conn(), &request(None))).await?;

	// Ctrl-C is the `0x03` byte, forwarded as keystrokes exactly as SSH forwards it. The
	// shell catches the resulting SIGINT and abandons whatever line was being typed; the
	// session itself must survive so the next command still runs.
	session.write_keystrokes(b"echo abandoned\r").await?;
	session.write_keystrokes(b"\x03").await?;

	session.write_keystrokes(b"echo after-interrupt\r").await?;

	let mut transcript = Vec::new();
	let mut found = false;
	for _ in 0..30 {
		let Some(output) = within(session.next_output()).await else {
			break;
		};
		match output {
			ShellOutput::Data(chunk) => transcript.extend_from_slice(&chunk),
			ShellOutput::Exit { .. } => {}
			ShellOutput::Error(reason) => return Err(eyre!("shell failed: {reason}")),
		}
		if String::from_utf8_lossy(&transcript).contains("after-interrupt") {
			found = true;
			break;
		}
	}
	assert!(
		found,
		"an interrupt keystroke ended the session, transcript {:?}",
		String::from_utf8_lossy(&transcript)
	);

	session.close().await?;
	Ok(())
}

#[tokio::test]
async fn an_interactive_session_reports_its_exit_code_last() -> Result<()> {
	testnet::init_tracing();
	let server = testnet::spawn_server_on(ALPN_SHELL, Shell::new()).await?;
	let mut session = within(shell::open(server.conn(), &request(None))).await?;

	// Typed, not requested: this is a shell that ends itself while the client is still
	// watching, which is the case a teardown has to get right.
	session
		.write_keystrokes(b"echo exit-code-marker\rexit 4\r")
		.await?;

	let mut transcript = Vec::new();
	let mut exit = Option::<u32>::None;
	while exit.is_none() {
		let Some(output) = within(session.next_output()).await else {
			break;
		};
		match output {
			ShellOutput::Data(chunk) => transcript.extend_from_slice(&chunk),
			ShellOutput::Exit { code } => exit = Some(code),
			ShellOutput::Error(reason) => return Err(eyre!("shell failed: {reason}")),
		}
	}

	let transcript = String::from_utf8_lossy(&transcript);
	assert_eq!(
		exit,
		Some(4),
		"a shell that exited 4 did not report it, transcript {transcript:?}"
	);
	assert!(
		transcript.contains("exit-code-marker"),
		"the output the shell printed before it exited was lost: {transcript:?}"
	);

	Ok(())
}
