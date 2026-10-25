use std::ffi::OsString;

use color_eyre::eyre::Result;

use super::{ExecCommand, ExecInput, ExecOutput, SIGKILL};

#[test]
fn exec_command_round_trips_through_wire_format() -> Result<()> {
	let command = ExecCommand {
		cmd: OsString::from("printf"),
		args: vec![OsString::from("%s"), OsString::from("hello")],
		stdin: Some(b"ignored".to_vec()),
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
	let inputs = [
		ExecInput::Stdin(b"hello\n".to_vec()),
		ExecInput::Signal(SIGKILL),
		ExecInput::Close,
	];
	let outputs = [
		ExecOutput::Stderr(b"failure".to_vec()),
		ExecOutput::Code(3),
		ExecOutput::RunError("no such program".to_owned()),
	];

	for input in inputs {
		assert_eq!(
			postcard::from_bytes::<ExecInput>(&postcard::to_allocvec(&input)?)?,
			input
		);
	}
	for output in outputs {
		assert_eq!(
			postcard::from_bytes::<ExecOutput>(&postcard::to_allocvec(&output)?)?,
			output
		);
	}
	Ok(())
}

/// Only Unix can tell that a signal rather than an exit ended a process, so only there
/// is there anything to check.
#[cfg(unix)]
#[test]
fn a_command_ended_by_a_signal_reports_that_signal() {
	use std::os::unix::process::ExitStatusExt as _;

	use super::SIGNAL_EXIT_BASE;

	// The raw status a process that `SIGKILL` ended leaves behind.
	let killed = std::process::ExitStatus::from_raw(i32::from(SIGKILL));
	assert_eq!(
		exit_code_for(killed),
		SIGNAL_EXIT_BASE + i32::from(SIGKILL),
		"an interrupted command has to report the signal that ended it"
	);
}

#[cfg(unix)]
#[test]
fn an_exit_code_is_reported_as_it_is() {
	use std::os::unix::process::ExitStatusExt as _;

	let exited = std::process::ExitStatus::from_raw(7 << 8);
	assert_eq!(exit_code_for(exited), 7);
}
