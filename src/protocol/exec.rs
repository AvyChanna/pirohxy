use core::fmt::Debug;
use std::{ffi::OsString, process::Stdio};

use color_eyre::eyre::Result;
use iroh::{
	endpoint::{Connection, RecvStream, SendStream},
	protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	process::Command,
};
use tracing::{debug, trace};

use crate::protoutils::{interproto::spawn_interactive, stream::ReadStructExt};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecOutput {
	Stdout(Vec<u8>),
	Stderr(Vec<u8>),
	Code(i32),
	RunError(String),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecInput {
	Stdin(Vec<u8>),
	Signal(u8),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommand {
	cmd: OsString,
	args: Vec<OsString>,
	stdin: Option<OsString>,
	env: Vec<(OsString, OsString)>,
	cwd: Option<String>,
}

/// A Exec protocol handler for Iroh that implements the `ProtocolHandler` trait.
/// It handles incoming connections and execs those commands.
#[derive(Debug, Copy, Clone, Default)]
pub struct Exec;

impl Exec {
	/// Creates a new `Exec` instance.
	#[must_use]
	pub fn new() -> Self {
		Self {}
	}
}

impl ProtocolHandler for Exec {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		let endpoint_id = conn.remote_id();
		trace!("Accepted connection from {}", endpoint_id.fmt_short());
		loop {
			let (s, r) = conn.accept_bi().await?;
			let _task = tokio::spawn(async move {
				if let Err(err) = handle_exec(r, s).await {
					debug!("Failed to process Exec stream: {}", err);
				}
			});
		}
	}
}

async fn handle_exec(mut r: RecvStream, s: SendStream) -> Result<()> {
	// Read the command first
	let cmd_vals: ExecCommand = r.read_struct().await?;

	let mut cmd = Command::new(&cmd_vals.cmd);
	#[expect(
		unused_results,
		reason = "Setting up the command with potential side effects, but we handle errors on spawn"
	)]
	{
		cmd.args(&cmd_vals.args)
			.env_clear()
			.envs(cmd_vals.env.iter().cloned())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::piped());

		if let Some(cwd) = &cmd_vals.cwd {
			cmd.current_dir(cwd);
		}
	}

	let mut child = cmd.spawn()?;

	let stdout = child.stdout.take().unwrap();
	let stderr = child.stderr.take().unwrap();
	let stdin = child.stdin.take().unwrap();

	// Spawn interactive message handler
	let (mut rx_input, tx_output) = spawn_interactive::<ExecInput, ExecOutput>(r, s).await?;

	// Stream stdout in chunks
	let tx_output_stdout = tx_output.clone();
	let _stdout_task = tokio::spawn(async move {
		let mut reader = stdout;
		let mut buffer = [0u8; 8192];
		loop {
			match reader.read(&mut buffer).await {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					let chunk = buffer[..n].to_vec();
					#[expect(
						let_underscore_drop,
						reason = "Intentionally ignoring send errors since it just means the receiver is gone"
					)]
					let _ = tx_output_stdout.send(ExecOutput::Stdout(chunk)).await;
				}
			}
		}
	});

	// Stream stderr in chunks
	let tx_output_stderr = tx_output.clone();
	let _stderr_task = tokio::spawn(async move {
		let mut reader = stderr;
		let mut buffer = [0u8; 8192];
		loop {
			match reader.read(&mut buffer).await {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					let chunk = buffer[..n].to_vec();
					#[expect(
						let_underscore_drop,
						reason = "Intentionally ignoring send errors since it just means the receiver is gone"
					)]
					let _ = tx_output_stderr.send(ExecOutput::Stderr(chunk)).await;
				}
			}
		}
	});

	// Handle input and forward to process stdin
	let _stdin_task = tokio::spawn(async move {
		let mut stdin = stdin;
		while let Some(msg) = rx_input.recv().await {
			match msg {
				ExecInput::Stdin(data) => {
					if stdin.write_all(&data).await.is_err() {
						break;
					}
				}
				ExecInput::Signal(_sig) => {
					// Signal handling would go here
				}
			}
		}
	});

	// Wait for process to complete
	let status = child.wait().await?;

	// Send exit code
	let exit_code = ExecOutput::Code(status.code().unwrap_or(-1));
	#[expect(
		let_underscore_drop,
		reason = "Intentionally ignoring send errors since it just means the receiver is gone"
	)]
	let _ = tx_output.send(exit_code).await;

	Ok(())
	// r.read
	// Command::new("cmd")
	//     .args(["/C", "echo hello"])
	//     .output()
	//     .expect("failed to execute process")
	// trace!(
	// 	"Processed command {:?} for target {}, result: {:?}",
	// 	cmd, target_addr, result
	// );
	// let _ = r.stop(0u8.into());
	// let _ = s.finish();

	// result
}

#[cfg(test)]
mod tests {
	use std::ffi::OsString;

	use color_eyre::eyre::Result;

	use super::{ExecCommand, ExecInput, ExecOutput};

	#[test]
	fn exec_command_round_trips_through_wire_format() -> Result<()> {
		let command = ExecCommand {
			cmd: OsString::from("printf"),
			args: vec![OsString::from("%s"), OsString::from("hello")],
			stdin: Some(OsString::from("ignored")),
			env: vec![(OsString::from("PATH"), OsString::from("/bin"))],
			cwd: Some("/tmp".to_owned()),
		};

		let bytes = postcard::to_allocvec(&command)?;
		let decoded = postcard::from_bytes::<ExecCommand>(&bytes)?;

		assert_eq!(decoded, command);
		Ok(())
	}

	#[test]
	fn exec_interactive_messages_round_trip_through_wire_format() -> Result<()> {
		let input = ExecInput::Stdin(b"hello\n".to_vec());
		let output = ExecOutput::Stderr(b"failure".to_vec());

		assert_eq!(
			postcard::from_bytes::<ExecInput>(&postcard::to_allocvec(&input)?)?,
			input
		);
		assert_eq!(
			postcard::from_bytes::<ExecOutput>(&postcard::to_allocvec(&output)?)?,
			output
		);
		Ok(())
	}
}
