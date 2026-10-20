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
		#[cfg(tokio_unstable)]
		{
			self.command.process_group(0);
		}

		#[cfg(not(tokio_unstable))]
		unsafe {
			use nix::unistd::{setpgid, Pid};
			use std::io::Error;
			self.command.pre_exec(|| {
				setpgid(Pid::this(), Pid::from_raw(0))
					.map_err(Error::from)
					.map(|_| ())
			});
		}

		self.command.spawn().map(AsyncGroupChild::new)
	}
}
