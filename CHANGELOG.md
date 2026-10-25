# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Sections
are moved from `Unreleased` to a version heading when a tag is cut - `cargo-dist` builds
the GitHub release notes from those headings.

## [Unreleased]

### Added

- `shell` protocol: an interactive PTY session over `portable-pty`, including ConPTY on
  Windows (`pirohxy shell <server>`).
- `file` protocol: `stat`, `ls`, `get`, `put`, `mkdir`, `rm` and `mv`, confined to the
  server's `--file-root` (`pirohxy file <server> <subcommand>`).
- Per-protocol grants under `auth/<endpoint-id>/<protocol>`, plus `auth/<endpoint-id>/all`.
  Grant files accept a protocol alias (`sftp`, `tty`, `forward`, `socks5`) as well as the
  canonical name, and an unrecognized name is reported instead of silently granting nothing.
- `integration-tests/cli_smoke.rs`: end-to-end coverage that drives the real binary against
  a real `pirohxy egress` across all seven channels, and `integration-tests/mux.rs` for
  concurrency and grants against one server.
- `docs/cli_help.md`, generated from the `clap` definitions with
  `cargo run -- generate-markdown-help`.
- A `Generated docs` CI job that regenerates `docs/cli_help.md` and fails when the committed
  copy differs, so the help cannot drift from the CLI.
- `CHANGELOG.md`, which `cargo-dist` uses for release notes.
- `protoutils::tasks::Tasks`: a tracked task set for the accept and serve loops. It reports the
  count in flight, waits for a free slot once it reaches `MAX_INFLIGHT_TASKS` (256), gives the
  running tasks a 5 s `drain` grace period before aborting what outlasts it, ends them at once with
  `abort_all`, and aborts what it holds when it is dropped.
- `docs/zellij.md`: running Zellij as the server-side terminal multiplexer through the existing
  `shell --cmd` and `exec` commands - detached sessions, headless pane control, layouts, and Zellij's
  web server carried by `forward`. Documentation only: no code, no dependency and no new flag.
- **The `shell` client is a terminal, not a pipe.** `run_shell` in `main.rs` puts the local terminal in
  raw mode (`RawModeGuard`, restored on drop), reads key, paste and resize events through `crossterm`,
  reports the real window size and keeps it current with `ShellInput::Resize`, and forwards keystrokes
  whole - arrow keys, function keys and pastes reach the session unchanged instead of being
  line-buffered. When this side is not a terminal (a pipe or a redirect) stdin is forwarded byte for
  byte, which is how the non-interactive tests drive it. `crossterm` is the only new dependency, and
  only its event/input side is used.


### Changed

- **`shell` sessions pass the mouse through like tmux and SSH.** `RawModeGuard` now also turns on mouse reporting (and turns it back off on drop), `spawn_terminal_reader` handles `Event::Mouse`, and a new `mouse_bytes` re-encodes each crossterm `MouseEvent` as the SGR (1006) sequence a terminal puts on the wire (`CSI <code> ; <col> ; <row> M` for a press, drag or motion and `... m` for a release, with Shift/Alt/Ctrl in bits 4/8/16 and 1-based coordinates) and forwards it as `ShellInput::Data`. A click, drag or wheel turn therefore reaches vim, htop or Zellij unchanged. Mouse capture stays on for the session; a program that never asks for the mouse never sees these bytes, because its terminal only emits them once the program's own enable sequence reaches it through the relayed pty output. No new dependency: it is the same `crossterm` event side already in use.
- **`shell` keystrokes are encoded like Zellij's.** `key_bytes` in `main.rs` now mirrors Zellij's own crossterm key encoder (`cast_crossterm_key` in `zellij-utils/src/input/mod.rs`), so a keystroke that reaches a shell session produces the same bytes it would in a Zellij session. The previous encoder packed each sequence into a `u32` and dropped anything it did not know; the new one covers the full set - control bytes (Ctrl-C stays `0x03`), function keys F1-F12, Shift+Tab (`ESC [ Z`), Alt/Meta (`ESC` + the character), `AltGr`, and modifier-aware CSI sequences for navigation and simple keys (`ESC [ 1 ; <mod> A`, `ESC [ n ; <mod> ~`, and the kitty `CSI u` form). Navigation keys now travel as `ESC [ A` rather than the old `ESC A`. No new dependency: it is the same `crossterm` event side already in use.
- **Ctrl-C in a `shell` session is a keystroke, not a session-killer.** The client no longer sends
  Ctrl-C as `ShellInput::Signal(SIGINT)`; it forwards the `0x03` byte a terminal puts on the wire for
  it, exactly as SSH does. The server's pty (the Unix line discipline, or ConPTY on Windows) turns that
  byte into `SIGINT` / `CTRL_C_EVENT` for the program running in the session, so Zellij detaches and a
  shell abandons the line being typed instead of the whole session dying. `ShellInput::Signal` is now
  only the out-of-band request to end a session - `exec` uses it with `SIGKILL` - and the shell client
  no longer has an interrupt path that ends the session.
- **File transfer streams in both directions.** `protocol::client::file::download` returns a
  `Download` - an `AsyncRead` that pulls the server's 64 KiB frames one at a time - instead of
  a `Vec<u8>` holding the whole file, and `upload` takes any `AsyncRead` instead of a slice, so
  `file get` and `file put` cost one chunk of buffer whatever the file size. The buffered
  convenience, `Download::read_to_end`, refuses to grow past `MAX_BUFFERED_DOWNLOAD_LEN`
  (64 MiB).
- `cargo nextest` is the test runner everywhere: `.config/nextest.toml` sets a 120 s slow
  timeout with no retries, the `ci` profile turns off fail-fast, and the `cli_smoke` tests
  run four at a time because each boots two iroh nodes and binds a loopback port.
- `build.rs` asks `git` for the commit id instead of linking `git2`, so `libgit2-sys` - a
  C build dependency - is gone from the dist targets, and the script now declares
  `rerun-if-changed` so the embedded commit hash cannot go stale.
- The CLI runs on a multi-thread tokio runtime; `current_thread` was only available because
  `rt-multi-thread` was not enabled on the `tokio` dependency.
- `spawn_interactive` no longer returns a `Result`: both pumps report their own failures,
  so there was nothing for callers to handle.
- `report_failure` in the reverse tunnel builds its error with `eyre!` instead of routing a
  message through `io::Error`.
- Identity errors name the key file and keep their cause: a missing `self.priv` reports
  ``no usable identity in <config dir>/self.priv - run `pirohxy init` if you have not yet``,
  a corrupt one reports ``<file> is not a valid z-base32 key``.
- Chunk-size constants are cross-referenced against `MAX_STRUCT_FRAME_LEN` with compile-time
  assertions.
- Test fixtures share one `TempDir` guard and one `init_tracing` helper in
  `protoutils::interproto::testnet`, so a crashed test no longer leaves scratch directories
  behind.
- The six server protocol handlers share one accept loop - `protoutils::accept::serve_bi_streams`
  and `serve_uni_streams`. Each handler now names its stream handler and a log label; the
  accepted-stream log line, the per-stream task and the failure log are unchanged.
- **Stream tasks are tracked instead of detached.** The two shared accept loops, the server reverse
  tunnel, the `socks5`, `forward` and `r-forward` clients, the `exec` output pumps and the `shell`
  PTY relay each hold a `Tasks` set. In-flight streams are counted, a loop that has taken on more
  than `MAX_INFLIGHT_TASKS` stops accepting until one of them finishes, and the tasks still running
  when the connection or the serve loop ends are drained for 5 s and then reset instead of running on
  until the process does. An exit signal aborts the `socks5`, `forward` and `r-forward` client tasks
  at once.
- **`spawn_interactive` returns an `Interactive` session, not a bare channel pair.** It owns the two
  pumps that move the messages, and `Interactive::close` ends them: queued output is written, the
  send stream is finished, and the inbound pump is stopped. Dropping it without closing aborts the
  pumps and resets the stream, so `exec` and `shell` close their wire explicitly and hand back every
  `Interactive::sender` clone first.
- **The `shell` session ends on its own terms.** Keystrokes and the shell's exit code used to travel
  on one channel, and the thread waiting for the child held a sender of its own, so that channel
  never closed: a session whose shell exited stayed blocked in `recv()` with the terminal still open,
  and the handler could only end it by aborting the task - which dropped the exit code before it
  ever reached the client. Input and exit now travel on separate channels, a `CancellationToken`
  reports that the shell is gone, the watcher blocks in `child.wait()` instead of polling
  `try_wait()` every 100 ms, and the relay is drained rather than aborted so the exit code is the
  last frame the client sees. `run_pty_session` is split into `open_pty`, `spawn_reader`,
  `spawn_watcher`, `feed_loop`, `reap` and `await_reader`.
- `main` dispatches through `run_command(&FileConfig, &Path, &Cli)`, which keeps `main` well
  inside clippy's `too_many_lines` ceiling when a subcommand is added.
- The CLI test fixtures no longer race for a loopback port, and they stop waiting the moment
  the command they started has exited: `integration-tests/common.rs` gained `ReservedPort`,
  `Task::check_alive`, `Task::wait_until_listening` and `start_bound`, which reserves a port,
  hands it to the command and retries on a fresh port if the bind was won elsewhere. A bind
  failure is reported in about 2 s instead of after the 120 s readiness timeout.

### Removed

- **The bare `auth/<endpoint-id>` grant file.** A plain file named after a peer used to
  imply SOCKS because it predates the other protocols. Every protocol is now granted the
  same way: a file named after the protocol inside `auth/<endpoint-id>/`, or `all` for
  everything. An identity with no directory under `auth/` is granted nothing, so an old
  bare file must be converted with `mkdir` and `touch` before it works again.

### Fixed

- **Ctrl-C killed a `shell` session.** Pressing Ctrl-C used to end the whole interactive session: the
  client sent `ShellInput::Signal(SIGINT)` and the server's `feed_loop` answered any `Signal` by
  terminating the session, so a full-screen program such as Zellij died instead of detaching. Ctrl-C is
  now the `0x03` keystroke byte (see *Changed*), the session survives it, and
  `an_interrupt_keystroke_does_not_end_the_session` (Unix) checks that a shell keeps running after one.
- **`exec` runs a command instead of answering a request.** `protocol/exec.rs`,
  `protocol/client/exec.rs` and `run_exec` in `main.rs`:
  - Output is written as it arrives, on the stream it belongs to, instead of being collected
    and printed when the command ends. A command that prints for a long time is visible while
    it runs, and a server-side `2>/dev/null` still keeps the two streams apart.
  - The local standard input is read byte for byte rather than line for line, so a pipe, a
    redirect and a terminal reach the command the same way, and the end of it closes the
    command's own standard input - which is what lets `cat` and every filter finish. The
    session no longer ends when the local input ends, because a pipe reaches EOF long before
    the command does.
  - Ctrl-C (and SIGTERM on Unix) ends the command on the server with `SIGKILL`, the one signal
    a command cannot catch, block or ignore, and the code it died with comes back as
    `128 + 9` - what a shell reports for it - and is mirrored locally. A second Ctrl-C ends
    this side without waiting. `ExecInput::Signal` now says so on the wire.
  - The server inherits `PATH` for the command unless the request carries a `PATH` of its own.
    `env_clear` left the command running with no path in its environment at all, so anything
    it looked for by name - a script's second program, a pipeline, a tool invoking another
    tool - could not be found.
- `integration-tests/common.rs` gained `Node::run_with_stdin`, which gives a `pirohxy` command
  a real pipe on its standard input. `exec_carries_the_local_stdin_to_the_command` drives it,
  and `exec_ends_a_running_command_when_this_side_is_interrupted` - Unix only, like the other
  terminal-behaviour tests - interrupts a running CLI and checks the code it exits with.

## [0.3.1] - 2025-09-13

### Changed

- Dependency updates, including iroh and clap.

## [0.3.0]

Earlier history is not written up here; see the tags and `git log` for it.

## [0.2.0]

See the tag.

## [0.1.0]

First release: SOCKS5 proxy over iroh.
