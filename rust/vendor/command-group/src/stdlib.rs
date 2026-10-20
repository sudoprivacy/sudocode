//! Implementation of process group extensions for the
//! standard library’s [`Command` type](std::process::Command).

use std::{
	io::Result,
	process::{Command, ExitStatus, Output},
};

use crate::{builder::CommandGroupBuilder, GroupChild};

#[doc(inline)]
pub use erased::ErasedChild;

#[cfg(target_family = "windows")]
mod windows;

#[cfg(target_family = "unix")]
mod unix;

pub(crate) mod child;
pub(crate) mod erased;

/// Extensions for [`Command`](std::process::Command) adding support for process groups.
pub trait CommandGroup {
	/// Executes the command as a child process group, returning a handle to it.
	///
	/// By default, stdin, stdout and stderr are inherited from the parent.
	///
	/// On Windows, this creates a job object instead of a POSIX process group.
	///
	/// # Examples
	///
	/// Basic usage:
	///
	/// ```no_run
	/// use std::process::Command;
	/// use command_group::CommandGroup;
	///
	/// Command::new("ls")
	///         .group_spawn()
	///         .expect("ls command failed to start");
	/// ```
	fn group_spawn(&mut self) -> Result<GroupChild> {
		self.group().spawn()
	}

	/// Converts the implementor into a [`CommandGroupBuilder`](crate::CommandGroupBuilder), which can be used to
	/// set flags that are not available on the `Command` type.
	fn group(&mut self) -> CommandGroupBuilder<std::process::Command>;

	/// Executes the command as a child process group, waiting for it to finish and
	/// collecting all of its output.
	///
	/// By default, stdout and stderr are captured (and used to provide the
	/// resulting output). Stdin is not inherited from the parent and any
	/// attempt by the child process to read from the stdin stream will result
	/// in the stream immediately closing.
	///
	/// On Windows, this creates a job object instead of a POSIX process group.
	///
	/// # Examples
	///
	/// ```should_panic
	/// use std::process::Command;
	/// use std::io::{self, Write};
	/// use command_group::CommandGroup;
	///
	/// let output = Command::new("/bin/cat")
	///                      .arg("file.txt")
	///                      .group_output()
	///                      .expect("failed to execute process");
	///
	/// println!("status: {}", output.status);
	/// io::stdout().write_all(&output.stdout).unwrap();
	/// io::stderr().write_all(&output.stderr).unwrap();
	///
	/// assert!(output.status.success());
	/// ```
	fn group_output(&mut self) -> Result<Output> {
		self.group_spawn()
			.and_then(|child| child.wait_with_output())
	}

	/// Executes a command as a child process group, waiting for it to finish and
	/// collecting its status.
	///
	/// By default, stdin, stdout and stderr are inherited from the parent.
	///
	/// On Windows, this creates a job object instead of a POSIX process group.
	///
	/// # Examples
	///
	/// ```should_panic
	/// use std::process::Command;
	/// use command_group::CommandGroup;
	///
	/// let status = Command::new("/bin/cat")
	///                      .arg("file.txt")
	///                      .group_status()
	///                      .expect("failed to execute process");
	///
	/// println!("process finished with: {}", status);
	///
	/// assert!(status.success());
	/// ```
	fn group_status(&mut self) -> Result<ExitStatus> {
		self.group_spawn().and_then(|mut child| child.wait())
	}
}

impl CommandGroup for Command {
	fn group(&mut self) -> CommandGroupBuilder<'_, Command> {
		CommandGroupBuilder::new(self)
	}
}
