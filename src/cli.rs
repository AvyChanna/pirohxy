use core::{
	net::{IpAddr, Ipv4Addr, SocketAddr},
	str::FromStr,
	time::Duration,
};
use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand, value_parser};
use color_eyre::eyre::{OptionExt, Result, ensure};
use directories::ProjectDirs;
use pirohxy::protoutils::HostPort;

use parser::parse_duration;

const ORGANIZATION_NAME: &str = "avychanna";
const APPLICATION_NAME: &str = "pirohxy";
const DEFAULT_CLI_HELP_PATH: &str = "docs/cli_help.md";

#[derive(Parser, Debug)]
#[command(about, version, long_version=include_str!(concat!(env!("OUT_DIR"), "/long-help.txt")))]
pub(super) struct Cli {
	#[command(subcommand)]
	pub(super) command: Commands,

	#[arg(short = 'c', long, value_name = "CONFIG-DIR", global = true)]
	config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigGetter {
	Path,
	Identity {
		#[arg(help = "identity name")]
		identity_name: Option<String>,
	},
}

/// The operations the `file` command exposes.
#[derive(Debug, Subcommand)]
pub(super) enum FileAction {
	#[command(about = "List a directory")]
	Ls {
		#[arg(default_value = ".")]
		path: String,
	},
	#[command(about = "Report the metadata of one path")]
	Stat { path: String },
	#[command(about = "Download a remote file")]
	Get {
		remote: String,
		#[arg(help = "local file to write, '-' for stdout")]
		local: PathBuf,
	},
	#[command(about = "Upload a local file")]
	Put {
		local: PathBuf,
		remote: String,
		#[arg(long)]
		append: bool,
	},
	#[command(about = "Create a directory and its parents")]
	Mkdir { path: String },
	#[command(about = "Remove a file or an empty directory")]
	Rm { path: String },
	#[command(about = "Move a path")]
	Mv { from: String, to: String },
}

#[derive(Subcommand, Debug)]
pub(super) enum Commands {
	#[command(about = "Run egress")]
	Egress {
		#[arg(long, default_value = "5s", value_parser = parse_duration)]
		server_req_timeout: Duration,
		#[arg(
			long,
			value_name = "DIR",
			help = "Directory the `file` protocol is confined to"
		)]
		file_root: Option<PathBuf>,
		#[arg(
			long,
			value_name = "PROGRAM",
			help = "Program to run for a shell session that asks for no command"
		)]
		shell: Option<String>,
	},
	#[command(about = "Connect to an egress server")]
	Socks5 {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(short = 'b', long = "bind", default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))]
		bind_addr: SocketAddr,
		#[arg(long, default_value = "10s", value_parser = parse_duration)]
		conn_timeout: Duration,
	},
	#[command(
		about = "Forward a local port to a target through the server",
		alias = "forward"
	)]
	Tunnel {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(
			short = 'b',
			long,
			default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
			help = "Local address to listen on"
		)]
		bind: SocketAddr,
		#[arg(help = "Target the server should connect to, as `host:port`")]
		target: HostPort,
	},
	#[command(
		about = "Make the server listen on a port and hand the connections back",
		alias = "rforward"
	)]
	RForward {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(help = "Address the server should listen on, as `host:port`")]
		bind: HostPort,
		#[arg(help = "Local address to bridge those connections to, as `host:port`")]
		local: HostPort,
	},
	#[command(about = "Run a command on the server")]
	Exec {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(
			help = "Program and its arguments. Everything from here on is handed to the server, flags included, so this side's own options have to come before the server name",
			num_args = 1..,
			trailing_var_arg = true,
			value_parser = value_parser!(String)
		)]
		command: Vec<String>,
		#[arg(long, value_name = "DIR", help = "Working directory on the server")]
		cwd: Option<String>,
		#[arg(
			long,
			value_name = "KEY=VALUE",
			help = "Variable to run the command with, repeatable"
		)]
		env: Vec<String>,
	},
	#[command(about = "Open an interactive shell on the server")]
	Shell {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[arg(long, help = "Program to run instead of the server's default shell")]
		cmd: Option<String>,
		#[arg(long, default_value = "xterm-256color")]
		term: String,
		#[arg(long, value_name = "DIR", help = "Working directory on the server")]
		cwd: Option<String>,
		#[arg(
			long,
			value_name = "KEY=VALUE",
			help = "Variable to run the shell with, repeatable"
		)]
		env: Vec<String>,
	},
	#[command(about = "Transfer files with the server", alias = "sftp")]
	File {
		#[arg(help = "server name, use `:node_id` to connect directly")]
		server: String,
		#[command(subcommand)]
		action: FileAction,
	},
	#[command(about = "Initialize an identity")]
	Init,
	#[command(about = "Print current configuration")]
	Conf {
		#[command(subcommand)]
		getter: ConfigGetter,
	},
	#[clap(hide = true)]
	GenerateMarkdownHelp {
		#[arg(value_parser=PathBuf::from_str, default_value=DEFAULT_CLI_HELP_PATH)]
		path: PathBuf,
	},
}

impl Cli {
	/// Returns the path to the configuration directory.
	/// If the `--config` argument is provided, it returns that path.
	/// Otherwise, it uses the default project directories.
	pub(super) fn get_config_path(&self) -> Result<PathBuf> {
		let cfg_dir = match &self.config {
			Some(path) => path.clone(),
			None => ProjectDirs::from("", ORGANIZATION_NAME, APPLICATION_NAME)
				.ok_or_eyre("Could not find project directories")?
				.config_local_dir()
				.to_path_buf(),
		};
		if !cfg_dir.exists() {
			fs::create_dir_all(&cfg_dir)?;
		}

		ensure!(
			cfg_dir.is_dir(),
			"Config path is not a directory: {}",
			cfg_dir.display()
		);
		Ok(cfg_dir.canonicalize()?)
	}
}

mod parser {
	use core::{num::ParseIntError, time::Duration};

	#[derive(Debug, PartialEq, thiserror::Error)]
	pub(crate) enum Error {
		#[error("invalid number format: {error}")]
		ParseErr { error: ParseIntError },
		#[error("minute to second conversion overflowed")]
		Overflow,
		#[error("duration string is empty")]
		Empty,
	}

	pub(crate) fn parse_duration(string: &str) -> Result<Duration, Error> {
		if string == "0" {
			return Ok(Duration::from_secs(0));
		}
		if string.is_empty() {
			return Err(Error::Empty);
		}

		if let Some(stripped) = string.strip_suffix('s') {
			let dur_num: u64 = stripped.parse().map_err(|e| Error::ParseErr { error: e })?;
			return Ok(Duration::from_secs(dur_num));
		}

		if let Some(stripped) = string.strip_suffix('m') {
			let dur_num: u64 = stripped.parse().map_err(|e| Error::ParseErr { error: e })?;
			let dur_sec = dur_num.checked_mul(60).ok_or(Error::Overflow)?;
			return Ok(Duration::from_secs(dur_sec));
		}

		let dur_num: u64 = string.parse().map_err(|e| Error::ParseErr { error: e })?;
		Ok(Duration::from_secs(dur_num))
	}
}

#[cfg(test)]
mod tests {
	use core::{
		net::{IpAddr, Ipv4Addr, SocketAddr},
		time::Duration,
	};
	use std::{
		fs,
		path::{Path, PathBuf},
	};

	use clap::Parser;
	use color_eyre::eyre::Result;

	use super::{
		Cli, Commands, ConfigGetter, FileAction,
		parser::{Error, parse_duration},
	};
	use pirohxy::protoutils::HostPort;

	fn test_config_path(name: &str) -> PathBuf {
		let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
		path.push("target");
		path.push("test-data");
		path.push(format!("cli-{name}-{}", std::process::id()));
		path
	}

	#[test]
	fn parses_duration_seconds_minutes_and_plain_seconds() {
		assert_eq!(parse_duration("0"), Ok(Duration::from_secs(0)));
		assert_eq!(parse_duration("15"), Ok(Duration::from_secs(15)));
		assert_eq!(parse_duration("15s"), Ok(Duration::from_secs(15)));
		assert_eq!(parse_duration("2m"), Ok(Duration::from_mins(2)));
	}

	#[test]
	fn rejects_empty_invalid_and_overflowing_durations() {
		assert_eq!(parse_duration(""), Err(Error::Empty));
		assert!(matches!(
			parse_duration("not-a-duration"),
			Err(Error::ParseErr { .. })
		));
		assert!(matches!(
			parse_duration("18446744073709551615000"),
			Err(Error::ParseErr { .. })
		));
		assert_eq!(
			parse_duration("18446744073709551615m"),
			Err(Error::Overflow)
		);
	}

	#[test]
	fn parses_connect_defaults() -> Result<()> {
		let cli = Cli::try_parse_from(["pirohxy", "socks5", "server-a"])?;

		let Commands::Socks5 {
			server,
			bind_addr,
			conn_timeout,
		} = cli.command
		else {
			panic!("expected connect command");
		};

		assert_eq!(server, "server-a");
		assert_eq!(
			bind_addr,
			SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
		);
		assert_eq!(conn_timeout, Duration::from_secs(10));
		Ok(())
	}
	#[test]
	fn parses_socks5_bind_and_timeout() -> Result<()> {
		let cli = Cli::try_parse_from([
			"pirohxy",
			"socks5",
			"srv",
			"--bind",
			"127.0.0.1:1080",
			"--conn-timeout",
			"30s",
		])?;
		let Commands::Socks5 {
			bind_addr,
			conn_timeout,
			..
		} = cli.command
		else {
			panic!("expected a socks5 command");
		};
		assert_eq!(
			bind_addr,
			SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1080)
		);
		assert_eq!(conn_timeout, Duration::from_secs(30));
		Ok(())
	}

	#[test]
	fn parses_config_getters() -> Result<()> {
		let path_cli = Cli::try_parse_from(["pirohxy", "conf", "path"])?;
		assert!(matches!(
			path_cli.command,
			Commands::Conf {
				getter: ConfigGetter::Path
			}
		));

		let identity_cli = Cli::try_parse_from(["pirohxy", "conf", "identity", "main"])?;
		assert!(matches!(
			identity_cli.command,
			Commands::Conf {
				getter: ConfigGetter::Identity {
					identity_name: Some(ref name)
				}
			} if name == "main"
		));
		Ok(())
	}

	#[test]
	fn get_config_path_creates_and_canonicalizes_explicit_directory() -> Result<()> {
		let path = test_config_path("explicit");
		if path.exists() {
			fs::remove_dir_all(&path)?;
		}

		let cli = Cli::try_parse_from([
			"pirohxy",
			"--config",
			path.to_str().expect("test path should be utf-8"),
			"init",
		])?;
		let actual = cli.get_config_path()?;

		assert!(actual.is_absolute());
		assert!(actual.is_dir());
		assert_eq!(actual, path.canonicalize()?);

		fs::remove_dir_all(path)?;
		Ok(())
	}

	#[test]
	fn parses_forward_and_rforward_aliases() -> Result<()> {
		let cli = Cli::try_parse_from(["pirohxy", "forward", "srv", "127.0.0.1:3128"])?;
		let Commands::Tunnel {
			server,
			bind,
			target,
		} = cli.command
		else {
			panic!("expected a tunnel command");
		};
		assert_eq!(server, "srv");
		assert_eq!(bind, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080));
		assert_eq!(target, HostPort::new("127.0.0.1", 3128));

		let cli =
			Cli::try_parse_from(["pirohxy", "rforward", "srv", "0.0.0.0:2222", "127.0.0.1:22"])?;
		let Commands::RForward {
			server,
			bind,
			local,
		} = cli.command
		else {
			panic!("expected a reverse tunnel command");
		};
		assert_eq!(server, "srv");
		assert_eq!(bind, HostPort::new("0.0.0.0", 2222));
		assert_eq!(local, HostPort::new("127.0.0.1", 22));
		Ok(())
	}

	#[test]
	fn parses_exec_with_the_flags_of_the_remote_command() -> Result<()> {
		let cli = Cli::try_parse_from(["pirohxy", "exec", "srv", "ls", "-l", "--color=always"])?;
		let Commands::Exec {
			server,
			command,
			cwd,
			env,
		} = cli.command
		else {
			panic!("expected an exec command");
		};
		assert_eq!(server, "srv");
		assert_eq!(command, ["ls", "-l", "--color=always"]);
		assert_eq!(cwd, None);
		assert_eq!(env, Vec::<String>::new());

		let cli = Cli::try_parse_from([
			"pirohxy", "exec", "--cwd", "/tmp", "--env", "FOO=bar", "srv", "sh", "-c", "echo hi",
		])?;
		let Commands::Exec {
			command, cwd, env, ..
		} = cli.command
		else {
			panic!("expected an exec command");
		};
		assert_eq!(command, ["sh", "-c", "echo hi"]);
		assert_eq!(cwd.as_deref(), Some("/tmp"));
		assert_eq!(env, ["FOO=bar"]);
		Ok(())
	}

	#[test]
	fn parses_shell_and_file_commands() -> Result<()> {
		let cli = Cli::try_parse_from([
			"pirohxy",
			"shell",
			"srv",
			"--cmd",
			"bash",
			"--term",
			"alacritty",
			"--cwd",
			"/srv",
			"--env",
			"FOO=bar",
			"--env",
			"EMPTY=",
		])?;
		let Commands::Shell {
			server,
			cmd,
			term,
			cwd,
			env,
		} = cli.command
		else {
			panic!("expected a shell command");
		};
		assert_eq!(server, "srv");
		assert_eq!(cmd.as_deref(), Some("bash"));
		assert_eq!(term, "alacritty");
		assert_eq!(cwd.as_deref(), Some("/srv"));
		assert_eq!(env, ["FOO=bar", "EMPTY="]);

		let cli = Cli::try_parse_from([
			"pirohxy",
			"sftp",
			"srv",
			"put",
			"notes.txt",
			"inbox/notes.txt",
			"--append",
		])?;
		let Commands::File { server, action } = cli.command else {
			panic!("expected a file command");
		};
		assert_eq!(server, "srv");
		assert!(matches!(
			action,
			FileAction::Put { ref local, ref remote, append: true }
				if local == Path::new("notes.txt") && remote == "inbox/notes.txt"
		));

		let cli = Cli::try_parse_from(["pirohxy", "file", "srv", "get", "notes.txt", "-"])?;
		let Commands::File { action, .. } = cli.command else {
			panic!("expected a file command");
		};
		assert!(matches!(action, FileAction::Get { ref local, .. } if local == Path::new("-")));
		Ok(())
	}
}
