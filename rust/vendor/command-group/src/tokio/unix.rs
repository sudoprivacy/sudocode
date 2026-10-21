use crate::builder::CommandGroupBuilder;
use crate::AsyncGroupChild;

impl CommandGroupBuilder<'_, tokio::process::Command> {
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
	/// use tokio::process::Command;
	/// use command_group::AsyncCommandGroup;
	///
	/// Command::new("ls")
	///         .group()
	///         .spawn()
	///         .expect("ls command failed to start");
	/// ```
	pub fn spawn(&mut self) -> std::io::Result<AsyncGroupChild> {
		use std::os::unix::process::CommandExt;

		// Share std's process-group configuration with the synchronous backend.
		// A pre_exec callback unnecessarily forces the fork/exec fallback.
		self.command.as_std_mut().process_group(0);

		self.command.spawn().map(AsyncGroupChild::new)
	}
}
