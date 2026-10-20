use std::{
	os::windows::{io::AsRawHandle, process::CommandExt},
	process::Command,
};
use winapi::um::winbase::CREATE_SUSPENDED;

use crate::{builder::CommandGroupBuilder, winres::*, GroupChild};

impl CommandGroupBuilder<'_, Command> {
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
	///         .group()
	/// 		.spawn()
	///         .expect("ls command failed to start");
	/// ```
	pub fn spawn(&mut self) -> std::io::Result<GroupChild> {
		self.command
			.creation_flags(self.creation_flags | CREATE_SUSPENDED);

		let (job, completion_port) = job_object(self.kill_on_drop)?;
		let child = self.command.spawn()?;
		assign_child(child.as_raw_handle(), job)?;

		Ok(GroupChild::new(child, job, completion_port))
	}
}
