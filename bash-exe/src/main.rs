//! BaoCode Bash (`bash.exe`): pi-shell's bash 5.3 for Windows, where an
//! agent's commands otherwise meet neither a bash nor the Unix tools it
//! expects.
//!
//! `bash -c COMMAND [NAME [ARGS]]`, what an agent's shell tool runs, runs
//! COMMAND as omp's bash tool does ([`pi_shell::execute_shell_stdio`]): the
//! GNU-style utilities built in (`rg`, `grep`, `sed`, `awk`, `find`, `xargs`,
//! `jq`, `tar`, …) ahead of `PATH`; a command line written for the Windows
//! program one of them shadows (`find /c "x"`, `sort /r`) going to that
//! program; `/c/…` and `C:\…` alike. Its fds 0, 1 and 2 are the process's
//! own. `-e`, `-u`, `-x` and `-l` may come with `-c`; the last does nothing,
//! there being no profile to read.
//!
//! Anything else (a script, commands on stdin, an interactive shell,
//! `--version`) is the engine as a process shell, the utilities standing in
//! for the commands `PATH` lacks; `--version` ends with this build's.

use std::{env, ffi::OsString, fs, io::Write as _, process::ExitCode};

use bstr::BString;
use pi_shell::{
	ShellExecuteOptions,
	cancel::{AbortReason, CancelToken},
	execute_shell_stdio,
	process::{self, ProcessOptions},
};

fn main() -> ExitCode {
	let args: Vec<OsString> = env::args_os().collect();
	let status = match CommandLine::parse(args.get(1..).unwrap_or_default()) {
		Some(line) => run(&line),
		None => process::main(
			args
				.iter()
				.map(|arg| BString::from(arg.to_string_lossy().into_owned())),
			ProcessOptions { extras: true, ..ProcessOptions::default() },
		),
	};
	// After bash's own lines (what scripts read the first of), which build
	// this is.
	if status == 0 && args.len() == 2 && args[1] == "--version" {
		println!("BaoCode Bash {} <{}>", env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_REPOSITORY"));
	}
	ExitCode::from((status & 0xff) as u8)
}

/// `bash [-c] [-e] [-u] [-x] [-l] [--login|--norc|--noprofile] [--] COMMAND
/// [NAME [ARGS]]`: a `-c` command line this runs itself.
#[derive(Debug, PartialEq, Eq)]
struct CommandLine {
	command: String,
	/// The `set` options given with it (`e`, `u`, `x`).
	options: String,
	/// `$0`.
	name:    Option<String>,
	/// `$1`, `$2`, ….
	args:    Vec<String>,
}

impl CommandLine {
	/// None for any other command line, or one not in Unicode: the process
	/// shell's.
	fn parse(args: &[OsString]) -> Option<Self> {
		let mut command = false;
		let mut options = String::new();
		let mut operands = Vec::new();
		let mut args = args.iter();
		while let Some(arg) = args.next() {
			let arg = arg.to_str()?;
			match arg {
				"--" => {
					operands.extend(args.by_ref().map(|a| a.to_str().map(str::to_owned)));
					break;
				},
				"--login" | "--norc" | "--noprofile" => {},
				_ if arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") => {
					for flag in arg[1..].chars() {
						match flag {
							'c' => command = true,
							'l' => {},
							'e' | 'u' | 'x' => options.push(flag),
							_ => return None,
						}
					}
				},
				_ if arg.starts_with('-') && arg != "-" => return None,
				_ => {
					operands.push(Some(arg.to_owned()));
					operands.extend(args.by_ref().map(|a| a.to_str().map(str::to_owned)));
					break;
				},
			}
		}
		if !command {
			return None;
		}
		let mut operands = operands
			.into_iter()
			.collect::<Option<Vec<_>>>()?
			.into_iter();
		Some(Self {
			command: operands.next()?,
			options,
			name: operands.next(),
			args: operands.collect(),
		})
	}

	/// The command with its options, `$0` and positional parameters set
	/// before it, on its first line (so that its line numbers stay its own).
	fn script(&self) -> String {
		let mut script = String::new();
		if !self.options.is_empty() {
			script.push_str("set -");
			script.push_str(&self.options);
			script.push_str("; ");
		}
		if let Some(name) = &self.name {
			script.push_str("BASH_ARGV0=");
			script.push_str(&quote(name));
			script.push_str("; ");
		}
		if !self.args.is_empty() {
			script.push_str("set --");
			for arg in &self.args {
				script.push(' ');
				script.push_str(&quote(arg));
			}
			script.push_str("; ");
		}
		script.push_str(&self.command);
		script
	}
}

/// [text] in single quotes, for the shell to read back as it is.
fn quote(text: &str) -> String {
	format!("'{}'", text.replace('\'', r"'\''"))
}

/// Runs [line] over this process's fds; its exit status, 130 when
/// interrupted.
fn run(line: &CommandLine) -> i32 {
	let runtime = match tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
	{
		Ok(runtime) => runtime,
		Err(err) => {
			eprintln!("bash: {err}");
			return 2;
		},
	};
	let status = runtime.block_on(async {
		let mut cancel = CancelToken::default();
		let abort = cancel.emplace_abort_token();
		tokio::spawn(async move {
			if tokio::signal::ctrl_c().await.is_ok() {
				abort.abort(AbortReason::Signal);
			}
		});
		let options = ShellExecuteOptions {
			command: line.script(),
			cwd: env::current_dir()
				.ok()
				.map(|dir| dir.to_string_lossy().into_owned()),
			..ShellExecuteOptions::default()
		};
		match execute_shell_stdio(options, own_stdio(), cancel).await {
			Ok(result) => result
				.exit_code
				.unwrap_or(if result.timed_out { 124 } else { 130 }),
			Err(err) => {
				eprintln!("bash: {err}");
				2
			},
		}
	});
	let _ = std::io::stdout().flush();
	// Background jobs it left running are not waited for, as bash's are not.
	runtime.shutdown_background();
	status
}

/// This process's stdin, stdout and stderr, each duplicated; None for one
/// it has not (a GUI parent's), the null device's then.
fn own_stdio() -> [Option<fs::File>; 3] {
	#[cfg(unix)]
	use std::os::fd::AsFd as AsOwn;
	#[cfg(windows)]
	use std::os::windows::io::AsHandle as AsOwn;

	fn own(stream: &impl AsOwn) -> Option<fs::File> {
		#[cfg(unix)]
		let owned = stream.as_fd().try_clone_to_owned();
		#[cfg(windows)]
		let owned = stream.as_handle().try_clone_to_owned();
		owned.ok().map(fs::File::from)
	}

	[own(&std::io::stdin()), own(&std::io::stdout()), own(&std::io::stderr())]
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(args: &[&str]) -> Option<CommandLine> {
		CommandLine::parse(&args.iter().map(OsString::from).collect::<Vec<_>>())
	}

	#[test]
	fn a_command_line_with_its_name_and_arguments() {
		let line = parse(&["-c", "echo \"$1\"", "bash", "it's"]).unwrap();
		assert_eq!(line.command, "echo \"$1\"");
		assert_eq!(line.script(), "BASH_ARGV0='bash'; set -- 'it'\\''s'; echo \"$1\"");
	}

	#[test]
	fn options_around_c() {
		let line = parse(&["-l", "-c", "-e", "true"]).unwrap();
		assert_eq!(line.options, "e");
		assert_eq!(line.script(), "set -e; true");
		assert_eq!(parse(&["-lc", "true"]).unwrap().command, "true");
		assert_eq!(parse(&["--login", "-c", "--", "-x"]).unwrap().command, "-x");
	}

	#[test]
	fn the_rest_is_the_process_shell() {
		assert_eq!(parse(&["script.sh"]), None);
		assert_eq!(parse(&["--version"]), None);
		assert_eq!(parse(&["-o", "pipefail", "-c", "true"]), None);
		assert_eq!(parse(&[]), None);
		assert_eq!(parse(&["-c"]), None);
	}
}
