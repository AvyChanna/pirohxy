use core::time::Duration;
use std::ffi::OsString;

use color_eyre::eyre::{Result, eyre};

use super::{Exec, ExecCommand, ExecOutput, SIGKILL};
use crate::protocol::client::exec;
use crate::protoutils::{alpn::ALPN_EXEC, interproto::testnet};

/// Fails the test instead of hanging when a step never completes.
async fn within<T>(step: impl Future<Output = T>) -> T {
	tokio::time::timeout(Duration::from_secs(20), step)
		.await
		.expect("exec step timed out")
}

/// Builds a command, keeping the parent `PATH` because the server clears the
/// environment before spawning.
fn command(cmd: &str, args: &[&str]) -> ExecCommand {
	ExecCommand {
		cmd: OsString::from(cmd),
		args: args.iter().map(OsString::from).collect(),
		stdin: None,
		env: std::env::var_os("PATH")
			.map_or_else(Vec::new, |path| vec![(OsString::from("PATH"), path)]),
		cwd: None,
	}
}

/// A command that copies its standard input to its standard output.
fn cat() -> ExecCommand {
	if cfg!(windows) {
		command("findstr.exe", &["."])
	} else {
		command("/bin/cat", &[])
	}
}

/// A command that exits with `code`.
fn exit_with(code: u8) -> ExecCommand {
	let script = format!("exit {code}");
	if cfg!(windows) {
		command("cmd.exe", &["/C", script.as_str()])
	} else {
		command("/bin/sh", &["-c", script.as_str()])
	}
}

/// A command that would keep running until something ends it.
///
/// The wait is never waited out: a server that cannot end the command fails the test on
/// its own timeout. Both of its streams go where the leftover parts of the command
/// cannot hold them open, so ending it really does end the output too.
fn linger() -> ExecCommand {
	if cfg!(windows) {
		command("cmd.exe", &["/C", "ping -n 120 127.0.0.1 >nul 2>nul"])
	} else {
		command("/bin/sleep", &["120"])
	}
}

#[tokio::test]
async fn client_exec_collects_stdout_and_the_exit_code() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;
	let command = if cfg!(windows) {
		command("cmd.exe", &["/C", "echo hello-from-exec"])
	} else {
		command("/bin/echo", &["hello-from-exec"])
	};

	let outcome = within(exec::run(server.conn(), &command)).await?;
	assert_eq!(outcome.code, Some(0));
	assert_eq!(outcome.error, None);
	assert!(String::from_utf8_lossy(&outcome.stdout).contains("hello-from-exec"));
	assert_eq!(String::from_utf8_lossy(&outcome.stderr), "");
	Ok(())
}

#[tokio::test]
async fn client_exec_reports_a_non_zero_exit_code() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;
	let outcome = within(exec::run(server.conn(), &exit_with(7))).await?;
	assert_eq!(outcome.code, Some(7));
	assert_eq!(outcome.error, None);
	Ok(())
}

#[tokio::test]
async fn client_exec_keeps_stdout_and_stderr_apart() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;
	let command = if cfg!(windows) {
		command("cmd.exe", &["/C", "echo to-stdout & echo to-stderr 1>&2"])
	} else {
		command("/bin/sh", &["-c", "echo to-stdout; echo to-stderr 1>&2"])
	};

	let outcome = within(exec::run(server.conn(), &command)).await?;
	let stdout = String::from_utf8_lossy(&outcome.stdout).into_owned();
	let stderr = String::from_utf8_lossy(&outcome.stderr).into_owned();
	assert!(stdout.contains("to-stdout") && !stdout.contains("to-stderr"));
	assert!(stderr.contains("to-stderr") && !stderr.contains("to-stdout"));
	Ok(())
}

#[tokio::test]
async fn client_exec_streams_stdin_to_the_command() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;

	let mut session = within(exec::open(server.conn(), &cat())).await?;
	session.write_stdin(b"typed-into-stdin\n").await?;
	let outcome = within(session.finish()).await?;

	assert!(String::from_utf8_lossy(&outcome.stdout).contains("typed-into-stdin"));
	assert_eq!(outcome.code, Some(0));
	Ok(())
}

#[tokio::test]
async fn client_exec_uses_the_stdin_supplied_with_the_command() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;

	let mut command = cat();
	command.stdin = Some(b"preloaded\n".to_vec());
	let outcome = within(exec::run(server.conn(), &command)).await?;

	assert!(String::from_utf8_lossy(&outcome.stdout).contains("preloaded"));
	assert_eq!(outcome.code, Some(0));
	Ok(())
}

#[tokio::test]
async fn client_exec_reports_a_command_it_cannot_start() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;
	let outcome = within(exec::run(
		server.conn(),
		&command("pirohxy-not-a-real-program", &[]),
	))
	.await?;

	assert!(outcome.error.is_some());
	assert_eq!(outcome.code, None);
	Ok(())
}

#[tokio::test]
async fn client_exec_ends_a_command_that_would_otherwise_run_on() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;

	let mut session = within(exec::open(server.conn(), &linger())).await?;
	session.signal(SIGKILL).await?;
	let outcome = within(session.finish()).await?;

	#[cfg(unix)]
	{
		use super::SIGNAL_EXIT_BASE;
		assert_eq!(
			outcome.code,
			Some(SIGNAL_EXIT_BASE + i32::from(SIGKILL)),
			"an interrupted command reported {:?} instead of the signal that ended it",
			outcome.code
		);
	}
	#[cfg(not(unix))]
	assert_ne!(
		outcome.code,
		Some(0),
		"an interrupted command reported {:?}",
		outcome.code
	);
	Ok(())
}

#[tokio::test]
async fn client_exec_ends_a_command_after_its_stdin_ended() -> Result<()> {
	let server = testnet::spawn_server_on(ALPN_EXEC, Exec).await?;
	// The command reads its input to the end and then keeps running, so the session has
	// to stay open past the end of the input for the interrupt to reach it. `exec` hands
	// the wait on to `sleep` so that nothing is left holding the output pipes open.
	let command = if cfg!(windows) {
		command(
			"cmd.exe",
			&["/C", "findstr . & ping -n 120 127.0.0.1 >nul 2>nul"],
		)
	} else {
		command("/bin/sh", &["-c", "cat; exec sleep 120"])
	};

	let mut session = within(exec::open(server.conn(), &command)).await?;
	session.write_stdin(b"typed-before-the-end\n").await?;
	session.close_stdin().await?;

	// The interrupt is sent only once the command has shown it got past its input, so
	// this tests the part that keeps running rather than racing the part that reads.
	let mut seen = String::new();
	while !seen.contains("typed-before-the-end") {
		match within(session.next_output()).await {
			Some(ExecOutput::Stdout(chunk)) => seen.push_str(&String::from_utf8_lossy(&chunk)),
			other => {
				return Err(eyre!(
					"the command stopped before the interrupt: {other:?} from {seen:?}"
				));
			}
		}
	}

	session.signal(SIGKILL).await?;
	let outcome = within(session.finish()).await?;

	// `seen` holds everything read before the interrupt; `outcome` holds only what came
	// after it.
	assert!(
		seen.contains("typed-before-the-end"),
		"the command never saw the input written before its end: {seen:?}"
	);
	#[cfg(unix)]
	{
		use super::SIGNAL_EXIT_BASE;
		assert_eq!(
			outcome.code,
			Some(SIGNAL_EXIT_BASE + i32::from(SIGKILL)),
			"an interrupted command reported {:?}",
			outcome.code
		);
	}
	#[cfg(not(unix))]
	assert_ne!(
		outcome.code,
		Some(0),
		"an interrupted command reported {:?}",
		outcome.code
	);
	Ok(())
}
