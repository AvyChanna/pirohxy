mod cfg;
mod cli;

use std::{
	ffi::OsString,
	fs,
	io::{Read, Write},
	path::Path,
	process::exit,
};

use clap::Parser;
use color_eyre::eyre::{Result, bail, eyre};
use crossterm::event::{
	Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
	read as read_event,
};
use crossterm::execute;
use iroh::endpoint::Connection;
use tokio::{
	io::AsyncReadExt,
	sync::mpsc::{self, Sender},
};
use tracing::{debug, level_filters::LevelFilter, warn};
use tracing_subscriber::{EnvFilter, fmt::time::LocalTime};

use cfg::FileConfig;
use cli::{Cli, Commands, ConfigGetter, FileAction};
use pirohxy::{
	protocol::{
		client::{connect::Client, exec, file, reverse_tunnel, shell, socks5, tunnel},
		exec::{ExecCommand, ExecOutput, SIGKILL},
		file::{FILE_CHUNK_LEN, FileMetadata},
		shell::{ShellOutput, ShellRequest},
	},
	protoutils::alpn::Protocol,
	start_egress,
};

// A multi-thread runtime: every accepted stream runs as its own task, and the work per
// stream is QUIC framing plus AEAD, so the process should use the cores it has. The PTY
// bridge already lives on the blocking pool, so it does not depend on this choice.
#[tokio::main]
async fn main() -> Result<()> {
	color_eyre::install()?;

	init_tracing();

	let cli = Cli::parse();
	debug!(args = ?cli, "Cli args");

	let cfg_dir = cli.get_config_path()?;
	let cfg = FileConfig::new(&cfg_dir)?;

	run_command(&cfg, &cfg_dir, &cli).await
}

/// Runs the subcommand the CLI selected.
///
/// The dispatch lives here rather than in `main`, so adding a subcommand costs one arm in a
/// function that has room for it instead of pushing `main` past clippy's `too_many_lines`
/// ceiling.
async fn run_command(cfg: &FileConfig, cfg_dir: &Path, cli: &Cli) -> Result<()> {
	match &cli.command {
		Commands::Egress {
			server_req_timeout,
			file_root,
			shell,
		} => {
			let self_key = cfg.load_identity()?;
			let file_root = file_root.clone().unwrap_or_else(|| cfg.file_root());
			fs::create_dir_all(&file_root)?;
			// The authorizer is kept by the router for as long as the server runs, so it
			// holds its own copy of the config instead of borrowing this one.
			let cfg = cfg.clone();
			start_egress(
				self_key,
				&file_root,
				shell.as_deref(),
				*server_req_timeout,
				move |id, protocol| cfg.is_access_allowed(&id, protocol),
			)
			.await
		}
		Commands::Socks5 {
			server,
			bind_addr,
			conn_timeout,
		} => {
			let self_key = cfg.load_identity()?;
			let server_key = cfg.resolve_server_name(server)?;
			socks5::bind_and_connect(self_key, server_key, bind_addr, *conn_timeout).await
		}
		Commands::Tunnel {
			server,
			bind,
			target,
		} => {
			let client = connect_to(cfg, server, Protocol::Tunnel).await?;
			let served = tunnel::serve(client.conn(), bind, target).await;
			client.close().await;
			served
		}
		Commands::RForward {
			server,
			bind,
			local,
		} => {
			let client = connect_to(cfg, server, Protocol::ReverseTunnel).await?;
			let served = reverse_tunnel::serve(client.conn(), bind, local).await;
			client.close().await;
			served
		}
		Commands::Exec {
			server,
			command,
			cwd,
			env,
		} => {
			let client = connect_to(cfg, server, Protocol::Exec).await?;
			let command = build_exec_command(command, cwd.as_deref(), env)?;
			let code = run_exec(client.conn(), &command).await;
			client.close().await;
			mirror_exit_code(code?)
		}
		Commands::Shell {
			server,
			cmd,
			term,
			cwd,
			env,
		} => {
			let client = connect_to(cfg, server, Protocol::Shell).await?;
			let served = run_shell(client.conn(), cmd.as_deref(), term, cwd.as_deref(), env).await;
			client.close().await;
			mirror_exit_code(served?)
		}
		Commands::File { server, action } => {
			let client = connect_to(cfg, server, Protocol::File).await?;
			let done = run_file_action(client.conn(), action).await;
			client.close().await;
			done
		}
		Commands::Init => cfg.init_identity(),
		Commands::Conf { getter } => report_config(cfg, cfg_dir, getter),
		Commands::GenerateMarkdownHelp { path } => generate_markdown_help(path),
	}
}

/// Sends logs to stderr, filtered by `RUST_LOG` and stamped with local times.
///
/// stderr rather than stdout: `file get <server> <path> -` writes the downloaded bytes to
/// stdout, and a log line in the middle of them would corrupt the file.
fn init_tracing() {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy(),
		)
		.with_timer(LocalTime::rfc_3339())
		.with_writer(std::io::stderr)
		.init();
}

/// Writes the `clap` help for this CLI out as markdown.
fn generate_markdown_help(path: &Path) -> Result<()> {
	debug!("Running md_help_gen with out path: {path:?}");
	let md_opts = clap_markdown::MarkdownOptions::new().show_footer(false);
	let md_str = clap_markdown::help_markdown_custom::<Cli>(&md_opts);
	let parent = path.parent().ok_or_else(|| {
		eyre!(
			"Doc generation failed: Could not find base path for file: {}",
			path.display()
		)
	})?;

	fs::create_dir_all(parent)?;
	Ok(fs::write(path, md_str)?)
}

/// Loads the local identity, resolves `server` to an id, and authenticates for
/// `protocol` over a throwaway node identity.
async fn connect_to(cfg: &FileConfig, server: &str, protocol: Protocol) -> Result<Client> {
	let self_key = cfg.load_identity()?;
	let server_key = cfg.resolve_server_name(server)?;
	Client::connect(self_key, server_key, protocol).await
}

/// Prints what the `conf` command asks for.
fn report_config(cfg: &FileConfig, cfg_dir: &Path, getter: &ConfigGetter) -> Result<()> {
	match getter {
		ConfigGetter::Path => {
			#[expect(clippy::print_stdout, reason = "user requested data on stdout")]
			{
				println!("{}", dunce::simplified(cfg_dir).display());
			}
			Ok(())
		}
		ConfigGetter::Identity {
			identity_name: None,
		} => {
			let self_key = cfg.load_identity()?;
			#[expect(clippy::print_stdout, reason = "user requested data on stdout")]
			{
				println!("{}", self_key.public().to_z32());
			}
			Ok(())
		}
		ConfigGetter::Identity {
			identity_name: Some(name),
		} => {
			let endpoint_id = cfg.resolve_server_name(name)?;
			#[expect(clippy::print_stdout, reason = "user requested data on stdout")]
			{
				println!("{}", endpoint_id.to_z32());
			}
			Ok(())
		}
	}
}

/// Splits `KEY=VALUE` arguments into pairs; a key with no `=` gets an empty value.
fn env_pairs(env: &[String]) -> impl Iterator<Item = (&str, &str)> {
	env.iter()
		.map(|pair| pair.split_once('=').unwrap_or((pair.as_str(), "")))
}

/// Builds the frame the server needs to start a command.
fn build_exec_command(
	command: &[String],
	cwd: Option<&str>,
	env: &[String],
) -> Result<ExecCommand> {
	let Some((program, args)) = command.split_first() else {
		return Err(eyre!("no command given"));
	};

	Ok(ExecCommand {
		cmd: OsString::from(program),
		args: args.iter().map(OsString::from).collect(),
		// Standard input stays open so the local terminal can feed the command.
		stdin: None,
		env: env_pairs(env)
			.map(|(key, value)| (OsString::from(key), OsString::from(value)))
			.collect(),
		cwd: cwd.map(str::to_owned),
	})
}

/// How many bytes of the local standard input go into one frame.
const STDIN_CHUNK_LEN: usize = 8192;

/// What this side can still give a command running on the server.
enum LocalInput {
	/// Bytes read from this side's standard input.
	Stdin(Vec<u8>),
	/// This side's standard input ended.
	StdinClosed,
}

/// Bridges one command on the server to this side's three standard streams.
///
/// Output is written as it arrives rather than collected, so a command that prints for a long
/// time is visible while it runs and `2>/dev/null` keeps the two streams apart. The local
/// standard input is read on a plain thread - byte for byte, not line for line - so a pipe, a
/// redirect and a terminal reach the command the same way, and its end is what closes the
/// command's own standard input. Ctrl-C ends the command on the server with [`SIGKILL`], which
/// a command cannot catch, so the code it died with is the code this side mirrors; a second
/// Ctrl-C stops this process without waiting.
///
/// Returns the code the command ended with, `None` when the channel just ended.
async fn run_exec(conn: &Connection, command: &ExecCommand) -> Result<Option<i32>> {
	let mut session = exec::open(conn, command).await?;

	let (tx_local, mut rx_local) = mpsc::channel::<LocalInput>(16);

	// A plain thread rather than a blocking task: the runtime waits for its blocking tasks
	// when it shuts down, and a terminal left open never ends its stdin read.
	let stdin_sender = tx_local.clone();
	let _stdin_reader = std::thread::spawn(move || {
		let mut stdin = std::io::stdin().lock();
		let mut buffer = vec![0u8; STDIN_CHUNK_LEN];
		loop {
			let chunk = match stdin.read(&mut buffer) {
				Ok(0) => break,
				Ok(read) => buffer[..read].to_vec(),
				Err(err) => {
					debug!("Local standard input stopped: {err}");
					break;
				}
			};
			if stdin_sender
				.blocking_send(LocalInput::Stdin(chunk))
				.is_err()
			{
				return;
			}
		}
		// The command has to see the end of its input, or a command that reads it to EOF -
		// `cat`, a filter - never finishes.
		let _sent = stdin_sender.blocking_send(LocalInput::StdinClosed);
	});

	let mut interrupt = std::pin::pin!(exec::interrupted());
	let mut exit_code = Option::<i32>::None;
	// Closing the local standard input must not end the session: a pipe reaches EOF long
	// before the command does, and dropping the session there throws away everything the
	// server still has to send.
	let mut local_open = true;
	// One interrupt is enough: after it the command is already dying.
	let mut interrupt_open = true;

	loop {
		tokio::select! {
			biased;
			output = session.next_output() => match output {
				Some(ExecOutput::Stdout(chunk)) => write_stream(&mut std::io::stdout(), &chunk)?,
				Some(ExecOutput::Stderr(chunk)) => write_stream(&mut std::io::stderr(), &chunk)?,
				Some(ExecOutput::Code(code)) => {
					exit_code = Some(code);
					break;
				}
				Some(ExecOutput::RunError(reason)) => {
					bail!("the server refused the command: {reason}");
				}
				None => break,
			},
			local = rx_local.recv(), if local_open => match local {
				Some(LocalInput::Stdin(data)) => {
					if session.write_stdin(&data).await.is_err() {
						warn!("The server stopped taking input for this command");
						local_open = false;
					}
				}
				Some(LocalInput::StdinClosed) => {
					if session.close_stdin().await.is_err() {
						local_open = false;
					}
				}
				None => local_open = false,
			},
			() = interrupt.as_mut(), if interrupt_open => {
				interrupt_open = false;
				debug!("Interrupted: ending the command on the server");
				if let Err(err) = session.signal(SIGKILL).await {
					debug!("Could not end the command on the server: {err}");
				}
			},
		}
	}

	// Closing the channel releases the reader thread as soon as its read returns, and
	// finishing the send stream tells the server no more input is coming.
	drop(tx_local);
	if let Err(err) = session.close_input() {
		debug!("Failed to finish the input stream: {err}");
	}

	Ok(exit_code)
}

/// Writes one chunk to a local stream and flushes it, so output appears while the command is
/// still running.
fn write_stream<W: Write>(stream: &mut W, data: &[u8]) -> Result<()> {
	stream.write_all(data)?;
	Ok(stream.flush()?)
}

/// Ends the process with the code the server reported, once the node is already closed.
fn mirror_exit_code(code: Option<i32>) -> Result<()> {
	match code {
		None | Some(0) => Ok(()),
		Some(code) => exit(code),
	}
}

/// The terminal size the server falls back to when this side cannot measure one.
const DEFAULT_COLS: u16 = 80;
/// See [`DEFAULT_COLS`].
const DEFAULT_ROWS: u16 = 24;

/// What the local terminal produced that the session has to act on.
enum TerminalEvent {
	/// Raw keystrokes to write to the session unchanged.
	///
	/// Ctrl-C arrives here too, as the `0x03` byte a terminal puts on the wire for it. That
	/// is how SSH forwards an interrupt: the byte goes into the pty and the server's line
	/// discipline turns it into `SIGINT` for the program running there, which is what lets
	/// Zellij detach and a shell abandon the line being typed instead of the session dying.
	Keystrokes(Vec<u8>),
	/// The local window changed size.
	Resize { cols: u16, rows: u16 },
	/// The local terminal ended or its reader failed.
	Finished,
}

/// Restores the terminal's modes when it is dropped.
///
/// Raw mode is set for the duration of a session; a session that ends through a panic, an
/// error or an early return still has to hand the terminal back to the shell that launched
/// it, so the restore lives in a drop rather than at the end of the happy path.
struct RawModeGuard {
	enabled: bool,
}

impl RawModeGuard {
	/// Puts the terminal into raw mode and turns on mouse reporting.
	///
	/// Mouse capture is left on for the whole session, which is what a terminal multiplexer
	/// effectively does once the program inside asks for the mouse: the program's own
	/// `CSI ? 1000 h`-style request reaches the local terminal through the relayed pty output,
	/// the terminal starts emitting mouse events, and `mouse_bytes` turns them back into the
	/// SGR sequence the program expects. A program that never asks for the mouse never sees
	/// these bytes, because its terminal only emits them when it is itself in mouse mode.
	/// Local echo is left as the terminal has it, so typed input still appears on this side
	/// exactly as the remote program echoes it.
	fn enable() -> Self {
		let enabled = crossterm::terminal::enable_raw_mode().is_ok();
		if !enabled {
			debug!("Could not enter raw mode; the session runs without it");
			return Self { enabled: false };
		}
		if execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).is_err() {
			debug!("Could not enable mouse capture; the session runs without it");
		}
		Self { enabled: true }
	}
}

impl Drop for RawModeGuard {
	fn drop(&mut self) {
		if self.enabled {
			let _result = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
			let _result = crossterm::terminal::disable_raw_mode();
		}
	}
}

/// The terminal size to ask the server for.
///
/// A real window size when this side can measure one, and the protocol default otherwise, so
/// a piped or redirected session still starts at a sane geometry.
fn terminal_size() -> (u16, u16) {
	match crossterm::terminal::size() {
		Ok((cols, rows)) => (cols.max(1), rows.max(1)),
		Err(_) => (DEFAULT_COLS, DEFAULT_ROWS),
	}
}

/// Encodes one key event as the bytes a terminal would put on the wire for it.
///
/// This mirrors Zellij's own crossterm key encoder (`cast_crossterm_key` in
/// `zellij-utils/src/input/mod.rs`), so a keystroke that reaches a piroxy shell session
/// produces the same bytes it would in a Zellij session: plain characters, control bytes
/// (Ctrl-C is `0x03`), function keys F1-F12, Shift+Tab (`ESC [ Z`), Alt/Meta (`ESC` + the
/// character), Ctrl+Alt and `AltGr`, and modifier-aware CSI sequences for navigation keys
/// (`ESC [ 1 ; <mod> A`, `ESC [ n ; <mod> ~`, and the kitty `CSI u` form for simple keys).
///
/// Returns `None` for events that carry no keystrokes - key releases, bare modifier presses,
/// media keys and the keyboard-enhancement keys that have no wire representation - so they are
/// not mistaken for input.
fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
	if key.kind == KeyEventKind::Release {
		return None;
	}
	let modifiers = key.modifiers;
	let has_ctrl = modifiers.contains(KeyModifiers::CONTROL);
	let has_alt = modifiers.contains(KeyModifiers::ALT);
	let has_shift = modifiers.contains(KeyModifiers::SHIFT);

	let is_char_key = matches!(key.code, KeyCode::Char(_));
	let bytes = match key.code {
		KeyCode::Char(c) => {
			// A character already carries the effect of Shift (and of AltGr, which Windows
			// reports as Ctrl+Alt), so those modifiers are not encoded on top of it. This is
			// what a terminal puts on the wire for the character the console reported.
			let is_altgr = has_ctrl && has_alt && !c.is_ascii_control();
			if is_altgr {
				c.encode_utf8(&mut [0u8; 4]).as_bytes().to_vec()
			} else if has_ctrl && has_alt && c.is_ascii_alphabetic() {
				// Ctrl+Alt+letter is Meta+Ctrl+letter: `ESC` then the control byte.
				vec![0x1b, (c.to_ascii_lowercase() as u8) & 0x1f]
			} else if has_ctrl && c.is_ascii_alphabetic() {
				// A control byte for the letter's position in the alphabet: Ctrl-A is 1,
				// Ctrl-Z is 26, which is what a terminal sends for it. Ctrl-C is `0x03`.
				vec![(c.to_ascii_lowercase() as u8) & 0x1f]
			} else if has_ctrl && c == ' ' {
				// Ctrl-Space is the NUL byte.
				vec![0x00]
			} else if has_alt {
				// Meta is the `ESC` prefix followed by the character.
				let mut bytes = vec![0x1b];
				bytes.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
				bytes
			} else {
				c.encode_utf8(&mut [0u8; 4]).as_bytes().to_vec()
			}
		}
		KeyCode::Null => vec![0x00],
		KeyCode::Enter => vec![0x0d],
		KeyCode::Tab => vec![0x09],
		KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
		KeyCode::Backspace => vec![0x7f],
		KeyCode::Esc => vec![0x1b],
		KeyCode::Left => vec![0x1b, b'[', b'D'],
		KeyCode::Right => vec![0x1b, b'[', b'C'],
		KeyCode::Up => vec![0x1b, b'[', b'A'],
		KeyCode::Down => vec![0x1b, b'[', b'B'],
		KeyCode::Home => vec![0x1b, b'[', b'H'],
		KeyCode::End => vec![0x1b, b'[', b'F'],
		KeyCode::PageUp => b"\x1b[5~".to_vec(),
		KeyCode::PageDown => b"\x1b[6~".to_vec(),
		KeyCode::Delete => b"\x1b[3~".to_vec(),
		KeyCode::Insert => b"\x1b[2~".to_vec(),
		KeyCode::F(n) => match n {
			1 => b"\x1bOP".to_vec(),
			2 => b"\x1bOQ".to_vec(),
			3 => b"\x1bOR".to_vec(),
			4 => b"\x1bOS".to_vec(),
			5 => b"\x1b[15~".to_vec(),
			6 => b"\x1b[17~".to_vec(),
			7 => b"\x1b[18~".to_vec(),
			8 => b"\x1b[19~".to_vec(),
			9 => b"\x1b[20~".to_vec(),
			10 => b"\x1b[21~".to_vec(),
			11 => b"\x1b[23~".to_vec(),
			12 => b"\x1b[24~".to_vec(),
			_ => return None,
		},
		// Caps/Scroll/Num Lock, Print Screen, Pause, Menu, KeypadBegin, media keys and bare
		// modifier presses have no keystroke bytes to forward.
		_ => return None,
	};

	// A character key already carries its modifiers in the bytes above, as does Shift+Tab,
	// which is a distinct key code that already encodes the shift. For every other key the
	// modifier flags have to be folded into the sequence, because crossterm reports them
	// separately from the bytes - otherwise the program on the server never sees them.
	let carries_its_modifiers = is_char_key || matches!(key.code, KeyCode::BackTab);
	if carries_its_modifiers || (!has_ctrl && !has_alt && !has_shift) {
		return Some(bytes);
	}
	Some(encode_modifiers(bytes, has_ctrl, has_alt, has_shift))
}

/// Folds the modifier flags crossterm reports for a non-character key into its escape
/// sequence, using the same forms Zellij uses: Alt alone prefixes `ESC`, everything else
/// goes through the CSI parameter or the kitty `CSI u` form.
fn encode_modifiers(bytes: Vec<u8>, has_ctrl: bool, has_alt: bool, has_shift: bool) -> Vec<u8> {
	// The xterm modifier code: 1 + shift + 2*alt + 4*ctrl.
	let modifier_code = 1 + u8::from(has_shift) + 2 * u8::from(has_alt) + 4 * u8::from(has_ctrl);
	let (first, second, third, last) = (
		bytes.first().copied(),
		bytes.get(1).copied(),
		bytes.get(2).copied(),
		bytes.last().copied(),
	);
	match (first, second, third, last) {
		// Alt alone on a simple key (Enter, Tab, Backspace) is the `ESC` prefix.
		(Some(b), _, _, _) if b != 0x1b && modifier_code == 3 => {
			let mut alt_bytes = vec![0x1b];
			alt_bytes.extend_from_slice(&bytes);
			alt_bytes
		}
		// A simple key with a non-Alt modifier is the kitty `CSI u` form.
		(Some(b), _, _, _) if b != 0x1b => format!("\x1b[{b};{modifier_code}u").into_bytes(),
		// A CSI letter-final sequence (`ESC [ A`) becomes `ESC [ 1 ; <mod> A`.
		(Some(0x1b), Some(b'['), Some(final_byte), _) if bytes.len() == 3 => {
			format!("\x1b[1;{modifier_code}{}", final_byte as char).into_bytes()
		}
		// A CSI tilde sequence (`ESC [ 5 ~`) becomes `ESC [ 5 ; <mod> ~`.
		(Some(0x1b), Some(b'['), _, Some(b'~')) if bytes.len() >= 4 => {
			let number = core::str::from_utf8(&bytes[2..bytes.len() - 1]).unwrap_or("1");
			format!("\x1b[{number};{modifier_code}~").into_bytes()
		}
		// An SS3 sequence (`ESC O P`, F1-F4) becomes `ESC [ 1 ; <mod> P`.
		(Some(0x1b), Some(b'O'), Some(final_byte), _) if bytes.len() == 3 => {
			format!("\x1b[1;{modifier_code}{}", final_byte as char).into_bytes()
		}
		// Anything else (Shift+Tab, a CSI tilde F-key like `ESC [ 15 ~`) is left as it is.
		_ => bytes,
	}
}

/// Encodes a mouse event as the SGR (1006) sequence a terminal puts on the wire for it.
///
/// This is the mouse counterpart of [`key_bytes`]: it turns the event crossterm parsed back
/// into the exact bytes the remote program asked its terminal to send, so a click, a drag or a
/// wheel turn reaches vim, htop or Zellij unchanged - the same passthrough SSH and tmux give.
/// The sequence is `CSI <code> ; <col> ; <row> M` for a press, drag or motion and `... m` for a
/// release, where `<code>` carries the button or wheel in its low bits and Shift, Alt and Ctrl in
/// bits 4, 8 and 16. Coordinates are 1-based, as the protocol counts them.
///
/// Every crossterm mouse kind has a wire form, so this always produces bytes.
fn mouse_bytes(event: MouseEvent) -> Vec<u8> {
	// The low two bits select the button; a press and its drag share the button code, and a
	// release is signalled by the trailing `m` instead of `M` rather than by a code.
	let (button, is_release) = match event.kind {
		MouseEventKind::Down(MouseButton::Left) => (0, false),
		MouseEventKind::Down(MouseButton::Middle) => (1, false),
		MouseEventKind::Down(MouseButton::Right) => (2, false),
		MouseEventKind::Up(MouseButton::Left) => (0, true),
		MouseEventKind::Up(MouseButton::Middle) => (1, true),
		MouseEventKind::Up(MouseButton::Right) => (2, true),
		// A drag is the button's press code with bit 5 (32) set; crossterm reports the button,
		// so the code is the same one a terminal sends for that drag.
		MouseEventKind::Drag(MouseButton::Left) => (32, false),
		MouseEventKind::Drag(MouseButton::Middle) => (33, false),
		MouseEventKind::Drag(MouseButton::Right) => (34, false),
		// A motion with no button held is code 35 in the SGR protocol.
		MouseEventKind::Moved => (35, false),
		MouseEventKind::ScrollUp => (64, false),
		MouseEventKind::ScrollDown => (65, false),
		MouseEventKind::ScrollLeft => (66, false),
		MouseEventKind::ScrollRight => (67, false),
	};
	let mut code = button;
	if event.modifiers.contains(KeyModifiers::SHIFT) {
		code |= 4;
	}
	if event.modifiers.contains(KeyModifiers::ALT) {
		code |= 8;
	}
	if event.modifiers.contains(KeyModifiers::CONTROL) {
		code |= 16;
	}
	// The protocol counts rows and columns from 1; crossterm reports them from 0.
	let column = u32::from(event.column) + 1;
	let row = u32::from(event.row) + 1;
	let terminator = if is_release { 'm' } else { 'M' };
	format!("\x1b[{code};{column};{row}{terminator}").into_bytes()
}
/// Reads the local terminal on a plain thread and hands what it produces to the session.
///
/// A terminal in raw mode is read as key, paste and resize events, so arrow keys, function
/// keys and pastes arrive whole and unbuffered rather than as line-buffered text; anything
/// else is read byte for byte, which keeps a piped or redirected stdin working. The thread
/// is plain rather than a blocking task because the runtime waits for blocking tasks on
/// shutdown, and a terminal left open never ends its read.
fn spawn_terminal_reader(sender: Sender<TerminalEvent>) {
	let _reader = std::thread::spawn(move || {
		if !crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
			let mut stdin = std::io::stdin().lock();
			let mut buffer = vec![0u8; STDIN_CHUNK_LEN];
			loop {
				match stdin.read(&mut buffer) {
					Ok(0) => break,
					Ok(read) => {
						if sender
							.try_send(TerminalEvent::Keystrokes(buffer[..read].to_vec()))
							.is_err()
						{
							return;
						}
					}
					Err(err) => {
						debug!("Local terminal stopped: {err}");
						break;
					}
				}
			}
			let _sent = sender.try_send(TerminalEvent::Finished);
			return;
		}

		loop {
			match read_event() {
				Ok(Event::Key(key)) => {
					// Ctrl-C is not special-cased: `key_bytes` already encodes it as the `0x03`
					// byte a terminal sends, which the server's pty turns into `SIGINT` for the
					// program running there. Forwarding the byte is what SSH does, and it is what
					// lets Zellij detach instead of the session being killed.
					if let Some(bytes) = key_bytes(key)
						&& sender.try_send(TerminalEvent::Keystrokes(bytes)).is_err()
					{
						return;
					}
				}
				Ok(Event::Paste(text)) => {
					if sender
						.try_send(TerminalEvent::Keystrokes(text.into_bytes()))
						.is_err()
					{
						return;
					}
				}
				Ok(Event::Mouse(mouse)) => {
					// A mouse event is forwarded as the SGR sequence the remote program asked
					// its terminal to send, so clicks, drags and wheel turns reach vim, htop or
					// Zellij unchanged - the passthrough SSH and tmux give.
					if sender
						.try_send(TerminalEvent::Keystrokes(mouse_bytes(mouse)))
						.is_err()
					{
						return;
					}
				}
				Ok(Event::Resize(cols, rows)) => {
					if sender
						.try_send(TerminalEvent::Resize {
							cols: cols.max(1),
							rows: rows.max(1),
						})
						.is_err()
					{
						return;
					}
				}
				Ok(_) => {}
				Err(err) => {
					debug!("Local terminal stopped: {err}");
					break;
				}
			}
		}
	});
}

/// Bridges the local terminal to a shell session on the server.
///
/// The local terminal is put into raw mode and read as key, paste and resize events, so this
/// side behaves like the terminal a program such as Zellij expects: arrow keys, function keys
/// and pastes reach the session whole, the real window size is reported and kept current, and
/// Ctrl-C is forwarded as the `0x03` byte the server's pty turns into `SIGINT`, exactly as SSH
/// does, so Zellij detaches and a shell abandons the typed line instead of the session dying.
/// When this side is not a terminal - a pipe or a redirect - stdin is forwarded byte for byte
/// instead, which is how the non-interactive tests drive it.
///
/// Returns the code the remote session exited with, `None` when the stream just ended.
async fn run_shell(
	conn: &Connection,
	cmd: Option<&str>,
	term: &str,
	cwd: Option<&str>,
	env: &[String],
) -> Result<Option<i32>> {
	let is_terminal = std::io::IsTerminal::is_terminal(&std::io::stdin())
		&& std::io::IsTerminal::is_terminal(&std::io::stdout());
	let (cols, rows) = terminal_size();
	let request = ShellRequest {
		cmd: cmd.map(str::to_owned),
		term: term.to_owned(),
		cols,
		rows,
		cwd: cwd.map(str::to_owned),
		env: env_pairs(env)
			.map(|(key, value)| (key.to_owned(), value.to_owned()))
			.collect(),
	};

	let mut session = shell::open(conn, &request).await?;

	// Raw mode is entered before the reader starts so the reader sees the same modes the
	// session runs under; the guard hands the terminal back whatever way this ends.
	let _raw = if is_terminal {
		RawModeGuard::enable()
	} else {
		RawModeGuard { enabled: false }
	};

	let (tx_event, mut rx_event) = mpsc::channel::<TerminalEvent>(16);
	spawn_terminal_reader(tx_event);

	let mut exit_code = Option::<i32>::None;
	// The last size already sent, so a burst of resize events collapses to the newest one
	// instead of flooding the session.
	let mut last_size = (cols, rows);
	// Closing the local terminal must not end the session: a piped or redirected stdin
	// reaches EOF long before the remote command finishes, and dropping the session there
	// would throw away everything the server still had to send.
	let mut terminal_open = true;

	loop {
		tokio::select! {
			biased;
			output = session.next_output() => match output {
				Some(ShellOutput::Data(data)) => {
					let mut stdout = std::io::stdout();
					stdout.write_all(&data)?;
					stdout.flush()?;
				}
				Some(ShellOutput::Exit { code }) => {
					exit_code = Some(i32::try_from(code).unwrap_or(i32::MAX));
					break;
				}
				Some(ShellOutput::Error(reason)) => {
					bail!("the server could not start the session: {reason}");
				}
				None => break,
			},
			event = rx_event.recv(), if terminal_open => {
				match event {
					Some(TerminalEvent::Keystrokes(data)) => {
						if session.write_keystrokes(&data).await.is_err() {
							warn!("The server stopped taking input for this session");
							terminal_open = false;
						}
					}
					Some(TerminalEvent::Resize { cols, rows }) => {
						if (cols, rows) != last_size {
							last_size = (cols, rows);
							if let Err(err) = session.resize(cols, rows).await {
								debug!("Could not resize the session: {err}");
							}
						}
					}
					Some(TerminalEvent::Finished) | None => terminal_open = false,
				}
			}
		}
	}

	// Dropping the receiver ends the channel, which releases the reader thread as soon as
	// its next send fails. The sender was moved into that thread.
	drop(rx_event);

	if let Err(err) = session.close().await {
		warn!("Failed to close the shell session cleanly: {err}");
	}

	Ok(exit_code)
}

/// Runs one `file` subcommand against the server.
async fn run_file_action(conn: &Connection, action: &FileAction) -> Result<()> {
	match action {
		FileAction::Ls { path } => {
			for entry in file::list_dir(conn, path).await? {
				print_entry(&entry);
			}
			Ok(())
		}
		FileAction::Stat { path } => {
			print_entry(&file::stat(conn, path).await?);
			Ok(())
		}
		FileAction::Get { remote, local } => {
			save_download(file::download(conn, remote, 0).await?, local).await
		}
		FileAction::Put {
			local,
			remote,
			append,
		} => {
			let mut source = tokio::fs::File::open(local).await?;
			file::upload(conn, remote, *append, &mut source).await
		}
		FileAction::Mkdir { path } => file::create_dir(conn, path).await,
		FileAction::Rm { path } => file::remove(conn, path).await,
		FileAction::Mv { from, to } => file::rename(conn, from, to).await,
	}
}

/// Copies a download into `local`, or into stdout when the local path is `-`.
///
/// The bytes move frame by frame, so a download of any size costs one chunk of buffer and
/// nothing but the file itself reaches stdout.
async fn save_download(mut download: file::Download, local: &Path) -> Result<()> {
	if local.as_os_str() == "-" {
		let mut stdout = std::io::stdout().lock();
		let mut buffer = vec![0u8; FILE_CHUNK_LEN];
		loop {
			let read = download.read(&mut buffer).await?;
			if read == 0 {
				break;
			}
			stdout.write_all(&buffer[..read])?;
		}
		stdout.flush()?;
		return Ok(());
	}

	let mut target = tokio::fs::File::create(local).await?;
	let copied = tokio::io::copy(&mut download, &mut target).await?;
	debug!("wrote {copied} bytes to {}", local.display());
	Ok(())
}

#[expect(
	clippy::print_stdout,
	reason = "the listing is what the user asked for"
)]
fn print_entry(entry: &FileMetadata) {
	let kind = if entry.is_dir {
		"d"
	} else if entry.is_symlink {
		"l"
	} else {
		"-"
	};
	println!("{kind} {:>12} {}", entry.size, entry.name);
}

#[cfg(test)]
mod tests {
	use super::{
		KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
		key_bytes, mouse_bytes,
	};

	fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
		KeyEvent {
			code,
			modifiers,
			kind: KeyEventKind::Press,
			state: crossterm::event::KeyEventState::NONE,
		}
	}

	#[test]
	fn a_plain_key_is_the_byte_a_terminal_sends_for_it() {
		assert_eq!(
			key_bytes(key(KeyCode::Char('a'), KeyModifiers::NONE)),
			Some(b"a".to_vec())
		);
		assert_eq!(
			key_bytes(key(KeyCode::Enter, KeyModifiers::NONE)),
			Some(vec![13])
		);
		assert_eq!(
			key_bytes(key(KeyCode::Esc, KeyModifiers::NONE)),
			Some(vec![27])
		);
	}

	#[test]
	fn a_control_key_is_its_control_byte() {
		assert_eq!(
			key_bytes(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
			Some(vec![3])
		);
		assert_eq!(
			key_bytes(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
			Some(vec![4])
		);
	}

	#[test]
	fn an_arrow_key_is_its_escape_sequence() {
		assert_eq!(
			key_bytes(key(KeyCode::Up, KeyModifiers::NONE)),
			Some(b"\x1b[A".to_vec())
		);
		assert_eq!(
			key_bytes(key(KeyCode::Left, KeyModifiers::NONE)),
			Some(b"\x1b[D".to_vec())
		);
	}

	#[test]
	fn a_function_key_is_its_escape_sequence() {
		assert_eq!(
			key_bytes(key(KeyCode::F(1), KeyModifiers::NONE)),
			Some(b"\x1bOP".to_vec())
		);
		assert_eq!(
			key_bytes(key(KeyCode::F(5), KeyModifiers::NONE)),
			Some(b"\x1b[15~".to_vec())
		);
		assert_eq!(
			key_bytes(key(KeyCode::F(12), KeyModifiers::NONE)),
			Some(b"\x1b[24~".to_vec())
		);
	}

	#[test]
	fn shift_tab_is_its_csi_sequence() {
		assert_eq!(
			key_bytes(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
			Some(b"\x1b[Z".to_vec())
		);
	}

	#[test]
	fn alt_is_the_escape_prefix_on_a_character() {
		assert_eq!(
			key_bytes(key(KeyCode::Char('a'), KeyModifiers::ALT)),
			Some(vec![0x1b, b'a'])
		);
	}

	#[test]
	fn control_alt_letter_is_altgr_on_a_console() {
		// On a console, Ctrl+Alt is AltGr: the character it produced is already translated,
		// so it is forwarded as the character itself, exactly as Zellij does.
		assert_eq!(
			key_bytes(key(
				KeyCode::Char('a'),
				KeyModifiers::CONTROL | KeyModifiers::ALT
			)),
			Some(b"a".to_vec())
		);
	}

	#[test]
	fn a_modified_arrow_carries_its_modifier_code() {
		// Ctrl+Up is `ESC [ 1 ; 5 A` (modifier code 1 + 4 for Ctrl).
		assert_eq!(
			key_bytes(key(KeyCode::Up, KeyModifiers::CONTROL)),
			Some(b"\x1b[1;5A".to_vec())
		);
		// Alt+Left is `ESC [ 1 ; 3 D` (modifier code 1 + 2 for Alt).
		assert_eq!(
			key_bytes(key(KeyCode::Left, KeyModifiers::ALT)),
			Some(b"\x1b[1;3D".to_vec())
		);
	}

	#[test]
	fn a_modified_tilde_key_carries_its_modifier_code() {
		// Ctrl+PageUp is `ESC [ 5 ; 5 ~`.
		assert_eq!(
			key_bytes(key(KeyCode::PageUp, KeyModifiers::CONTROL)),
			Some(b"\x1b[5;5~".to_vec())
		);
	}

	#[test]
	fn a_key_release_carries_no_keystrokes() {
		let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
		release.kind = KeyEventKind::Release;
		assert_eq!(key_bytes(release), None);
	}

	fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
		MouseEvent {
			kind,
			column,
			row,
			modifiers,
		}
	}

	#[test]
	fn a_left_click_is_the_sgr_press_sequence() {
		// Coordinates are 1-based on the wire, so crossterm's (0, 0) is `1;1`.
		assert_eq!(
			mouse_bytes(mouse(
				MouseEventKind::Down(MouseButton::Left),
				0,
				0,
				KeyModifiers::NONE
			)),
			b"\x1b[0;1;1M".to_vec()
		);
		assert_eq!(
			mouse_bytes(mouse(
				MouseEventKind::Down(MouseButton::Right),
				9,
				4,
				KeyModifiers::NONE
			)),
			b"\x1b[2;10;5M".to_vec()
		);
	}

	#[test]
	fn a_release_ends_with_a_lower_case_m() {
		assert_eq!(
			mouse_bytes(mouse(
				MouseEventKind::Up(MouseButton::Left),
				3,
				2,
				KeyModifiers::NONE
			)),
			b"\x1b[0;4;3m".to_vec()
		);
	}

	#[test]
	fn a_drag_carries_the_motion_bit() {
		// A drag is the button code with bit 5 (32) set.
		assert_eq!(
			mouse_bytes(mouse(
				MouseEventKind::Drag(MouseButton::Left),
				0,
				0,
				KeyModifiers::NONE
			)),
			b"\x1b[32;1;1M".to_vec()
		);
	}

	#[test]
	fn a_motion_with_no_button_is_code_35() {
		assert_eq!(
			mouse_bytes(mouse(MouseEventKind::Moved, 5, 5, KeyModifiers::NONE)),
			b"\x1b[35;6;6M".to_vec()
		);
	}

	#[test]
	fn a_wheel_turn_is_its_sgr_code() {
		assert_eq!(
			mouse_bytes(mouse(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE)),
			b"\x1b[64;1;1M".to_vec()
		);
		assert_eq!(
			mouse_bytes(mouse(MouseEventKind::ScrollDown, 0, 0, KeyModifiers::NONE)),
			b"\x1b[65;1;1M".to_vec()
		);
	}

	#[test]
	fn mouse_modifiers_are_the_protocol_bits() {
		// Shift is bit 4, Ctrl is bit 16: Ctrl+Shift+left-click is code 0 | 4 | 16 = 20.
		assert_eq!(
			mouse_bytes(mouse(
				MouseEventKind::Down(MouseButton::Left),
				0,
				0,
				KeyModifiers::SHIFT | KeyModifiers::CONTROL
			)),
			b"\x1b[20;1;1M".to_vec()
		);
		// Alt is bit 8.
		assert_eq!(
			mouse_bytes(mouse(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::ALT)),
			b"\x1b[72;1;1M".to_vec()
		);
	}
}
