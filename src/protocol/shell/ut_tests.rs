use core::fmt::Debug;
use core::time::Duration;

use color_eyre::eyre::Result;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::{
	DEFAULT_COLS, DEFAULT_ROWS, REAP_GRACE, Serialize, ShellInput, ShellOutput, ShellRequest,
	default_shell_program, interactive_shell_args, resolve_shell, run_pty_session,
	run_through_shell,
};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> Result<T> {
	Ok(postcard::from_bytes::<T>(&postcard::to_allocvec(value)?)?)
}

#[test]
fn shell_messages_round_trip_through_wire_format() -> Result<()> {
	let request = ShellRequest {
		cmd: Some("ls -la | head".to_owned()),
		term: "xterm-256color".to_owned(),
		cols: 120,
		rows: 40,
		cwd: Some("/tmp".to_owned()),
		env: vec![("LANG".to_owned(), "C".to_owned())],
	};
	let inputs = [
		ShellInput::Data(b"ls\r\n".to_vec()),
		ShellInput::Resize {
			cols: 100,
			rows: 30,
		},
		ShellInput::Signal(2),
		ShellInput::Close,
	];
	let outputs = [
		ShellOutput::Data(b"output".to_vec()),
		ShellOutput::Exit { code: 0 },
		ShellOutput::Error("no pty".to_owned()),
	];

	assert_eq!(round_trip(&request)?, request);
	for input in &inputs {
		assert_eq!(round_trip(input)?, *input);
	}
	for output in &outputs {
		assert_eq!(round_trip(output)?, *output);
	}
	Ok(())
}

#[test]
fn a_requested_command_line_is_run_through_the_shell() {
	let request = sample_request(Some("echo hi".to_owned()));
	let (program, args) = resolve_shell(&request, Some("/bin/sh"));
	let command_flag = if cfg!(unix) { "-c" } else { "/C" };
	assert_eq!(program, "/bin/sh");
	assert_eq!(args, [command_flag.to_owned(), "echo hi".to_owned()]);
}

#[test]
fn an_empty_command_line_starts_the_default_shell() {
	let request = sample_request(None);
	let (program, args) = resolve_shell(&request, Some("/bin/zsh"));
	assert_eq!(program, "/bin/zsh");
	assert_eq!(args, interactive_shell_args());

	let (program, _) = resolve_shell(&request, None);
	assert_eq!(program, default_shell_program());
}

#[test]
fn command_lines_keep_their_spacing() {
	let (_, args) = run_through_shell("/bin/sh", "echo  a  b");
	assert!(args.iter().any(|arg| arg == "echo  a  b"));
}

#[test]
fn pty_session_prints_the_output_of_a_requested_command() {
	let transcript = pty_transcript(Some("echo probe-marker".to_owned()));
	assert!(
		transcript.contains("probe-marker"),
		"the pty produced {transcript:?}"
	);
}

/// Runs a pty session on its own thread and returns everything it printed.
///
/// Nothing is typed into it: the command is the one the request carries, and the test
/// only lets go of the input channel once the shell has exited, which is the order a
/// real session ends in.
fn pty_transcript(cmd: Option<String>) -> String {
	let (tx_input, rx_input) = std::sync::mpsc::channel::<ShellInput>();
	let (tx_exit, rx_exit) = std::sync::mpsc::channel::<u32>();
	let (tx_output, mut rx_output) = tokio::sync::mpsc::unbounded_channel::<ShellOutput>();
	let watcher_token = CancellationToken::new();
	let session_token = watcher_token.clone();
	let mut tx_input = Some(tx_input);
	let session = std::thread::spawn(move || {
		run_pty_session(
			&sample_request(cmd),
			None,
			&rx_input,
			tx_exit,
			&rx_exit,
			session_token,
			&tx_output,
		)
	});

	let mut transcript = Vec::new();
	let deadline = std::time::Instant::now() + Duration::from_secs(15);
	while std::time::Instant::now() < deadline {
		match rx_output.try_recv() {
			Ok(ShellOutput::Data(chunk)) => transcript.extend_from_slice(&chunk),
			Ok(ShellOutput::Exit { code }) => {
				transcript.extend_from_slice(format!("<exit {code}>").as_bytes());
				break;
			}
			Ok(ShellOutput::Error(reason)) => {
				transcript.extend_from_slice(format!("<err {reason}>").as_bytes());
				break;
			}
			Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
				// The session only tears the terminal down once the input channel is
				// gone, and the shell exiting is what says it is safe to let go of it.
				if watcher_token.is_cancelled() {
					drop(tx_input.take());
				} else {
					std::thread::sleep(Duration::from_millis(50));
				}
			}
			Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
		}
	}
	drop(tx_input);
	drop(session.join());
	String::from_utf8_lossy(&transcript).into_owned()
}

/// A client that vanishes mid-session must not leave the session, or its shell, running.
#[test]
fn a_session_whose_client_vanished_ends_its_shell() {
	let (tx_input, rx_input) = std::sync::mpsc::channel::<ShellInput>();
	let (tx_exit, rx_exit) = std::sync::mpsc::channel::<u32>();
	let (tx_output, mut rx_output) = tokio::sync::mpsc::unbounded_channel::<ShellOutput>();
	let session_tx_output = tx_output.clone();
	let session = std::thread::spawn(move || {
		run_pty_session(
			&sample_request(None),
			None,
			&rx_input,
			tx_exit,
			&rx_exit,
			CancellationToken::new(),
			&session_tx_output,
		)
	});

	drop(tx_input);
	let ended = std::time::Instant::now();
	drop(session.join());
	drop(tx_output);

	// The code the session reports is the one its killed shell gave it, not the one it
	// falls back to after waiting out `REAP_GRACE` for a child that will not die.
	assert!(
		ended.elapsed() < REAP_GRACE,
		"a session with no client waited out its whole reap grace"
	);
	assert!(
		rx_output.try_recv().is_ok(),
		"a session with no client reported nothing"
	);
}

fn sample_request(cmd: Option<String>) -> ShellRequest {
	ShellRequest {
		cmd,
		term: "xterm".to_owned(),
		cols: DEFAULT_COLS,
		rows: DEFAULT_ROWS,
		cwd: None,
		env: Vec::new(),
	}
}
