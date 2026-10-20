use std::{
	fmt,
	io::Result,
	process::{ExitStatus, Output},
};

use tokio::{io::AsyncReadExt, process::Child};

#[cfg(unix)]
pub(self) use unix::ChildImp;
#[cfg(windows)]
pub(self) use windows::ChildImp;

#[cfg(unix)]
use nix::sys::signal::Signal;

#[cfg(windows)]
use winapi::um::winnt::HANDLE;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// Representation of a running or exited child process group (Tokio variant).
///
/// This wraps Tokio’s [`Child`] type with methods that work with process groups.
///
/// # Examples
///
/// ```should_panic
/// # #[tokio::main]
/// # async fn main() {
/// use tokio::process::Command;
/// use command_group::AsyncCommandGroup;
///
/// let mut child = Command::new("/bin/cat")
///                         .arg("file.txt")
///                         .group_spawn()
///                         .expect("failed to execute child");
///
/// let ecode = child.wait()
///                  .await
///                  .expect("failed to wait on child");
///
/// assert!(ecode.success());
/// # }
/// ```
pub struct AsyncGroupChild {
	imp: ChildImp,
	exitstatus: Option<ExitStatus>,
}

impl fmt::Debug for AsyncGroupChild {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.debug_struct("AsyncGroupChild").finish()
	}
}

impl AsyncGroupChild {
	#[cfg(unix)]
	pub(crate) fn new(inner: Child) -> Self {
		Self {
			imp: ChildImp::new(inner),
			exitstatus: None,
		}
	}

	#[cfg(windows)]
	pub(crate) fn new(inner: Child, j: HANDLE, c: HANDLE) -> Self {
		Self {
			imp: ChildImp::new(inner, j, c),
			exitstatus: None,
		}
	}

	/// Returns the stdlib [`Child`] object.
	///
	/// Note that the inner child may not be in the same state as this output child, due to how
	/// methods like `wait` and `kill` are implemented. It is not recommended to use this method
	/// _after_ using any of the other methods on this struct.
	///
	/// # Examples
	///
	/// Reading from stdout:
	///
	/// ```no_run
	/// # #[tokio::main]
	/// # async fn main() {
	/// use std::process::Stdio;
	/// use tokio::{io::AsyncReadExt, process::Command};
	/// use command_group::AsyncCommandGroup;
	///
	/// let mut child = Command::new("ls").stdout(Stdio::piped()).group_spawn().expect("ls command didn't start");
	/// let mut output = String::new();
	/// if let Some(mut out) = child.inner().stdout.take() {
	///     out.read_to_string(&mut output).await.expect("failed to read from child");
	/// }
	/// println!("output: {}", output);
	/// # }
	/// ```
	pub fn inner(&mut self) -> &mut Child {
		self.imp.inner()
	}

	/// Consumes itself and returns the stdlib [`Child`] object.
	///
	/// Note that the inner child may not be in the same state as this output child, due to how
	/// methods like `wait` and `kill` are implemented. It is not recommended to use this method
	/// _after_ using any of the other methods on this struct.
	///
	#[cfg_attr(
		windows,
		doc = "On Windows, this unnavoidably leaves a handle unclosed. Prefer [`inner()`](Self::inner)."
	)]
	///
	/// # Examples
	///
	/// Writing to input:
	///
	/// ```no_run
	/// # #[tokio::main]
	/// # async fn main() {
	/// use std::process::Stdio;
	/// use tokio::{io::AsyncWriteExt, process::Command};
	/// use command_group::AsyncCommandGroup;
	///
	/// let mut child = Command::new("cat").stdin(Stdio::piped()).group_spawn().expect("cat command didn't start");
	/// if let Some(mut din) = child.into_inner().stdin.take() {
	///      din.write_all(b"Woohoo!").await.expect("failed to write");
	/// }
	/// # }
	/// ```
	pub fn into_inner(self) -> Child {
		self.imp.into_inner()
	}

	/// Forces the child process group to exit.
	///
	/// If the group has already exited, an [`InvalidInput`] error is returned.
	///
	/// This is equivalent to sending a SIGKILL on Unix platforms.
	///
	/// See [the Tokio documentation](Child::kill) for more.
	///
	/// # Examples
	///
	/// Basic usage:
	///
	/// ```no_run
	/// # #[tokio::main]
	/// # async fn main() {
	/// use tokio::process::Command;
	/// use command_group::AsyncCommandGroup;
	///
	/// let mut command = Command::new("yes");
	/// if let Ok(mut child) = command.group_spawn() {
	///     child.kill().await.expect("command wasn't running");
	/// } else {
	///     println!("yes command didn't start");
	/// }
	/// # }
	/// ```
	///
	/// [`InvalidInput`]: std::io::ErrorKind::InvalidInput
	pub async fn kill(&mut self) -> Result<()> {
		self.start_kill()?;
		self.wait().await?;
		Ok(())
	}

	/// Attempts to force the child to exit, but does not wait for the request to take effect.
	///
	/// This is equivalent to sending a SIGKILL on Unix platforms.
	///
	/// Note that on Unix platforms it is possible for a zombie process to remain after a kill is
	/// sent; to avoid this, the caller should ensure that either `child.wait().await` or
	/// `child.try_wait()` is invoked successfully.
	///
	/// See [the Tokio documentation](Child::start_kill) for more.
	///
	/// [`InvalidInput`]: std::io::ErrorKind::InvalidInput
	pub fn start_kill(&mut self) -> Result<()> {
		self.imp.start_kill()
	}

	/// Returns the OS-assigned process group identifier.
	///
	/// Like Tokio, this returns `None` if the child process group has alread exited, to avoid
	/// holding onto an expired (and possibly reused) PGID.
	///
	/// See [the Tokio documentation](Child::id) for more.
	///
	/// # Examples
	///
	/// Basic usage:
	///
	/// ```no_run
	/// # #[tokio::main]
	/// # async fn main() {
	/// use tokio::process::Command;
	/// use command_group::AsyncCommandGroup;
	///
	/// let mut command = Command::new("ls");
	/// if let Ok(child) = command.group_spawn() {
	///     if let Some(pgid) = child.id() {
	///         println!("Child group's ID is {}", pgid);
	///     } else {
	///         println!("Child group is gone");
	///     }
	/// } else {
	///     println!("ls command didn't start");
	/// }
	/// # }
	/// ```
	pub fn id(&self) -> Option<u32> {
		self.imp.id()
	}

	/// Waits for the child group to exit completely, returning the status that the process leader
	/// exited with.
	///
	/// See [the Tokio documentation](Child::wait) for more.
	///
	/// The current implementation spawns a blocking task on the Tokio thread pool; contributions
	/// are welcome for a better version.
	///
	/// An important consideration on Unix platforms is that there is no way to cancel the `wait`
	/// syscall. _Cancelling this future_ will **not** cancel that underlying `wait` call. That has
	/// consequences: a `wait`ed process that exits will have its resources cleaned up by the kernel.
	/// If the application is no longer listening for that `wait` returning, it will not know that
	/// the process has been cleaned up, and will try to wait on it again. That in turn may fail, or
	/// could even attach to a recycled PID which would then point to a completely different process.
	///
	/// # Examples
	///
	/// Basic usage:
	///
	/// ```no_run
	/// # #[tokio::main]
	/// # async fn main() {
	/// use tokio::process::Command;
	/// use command_group::AsyncCommandGroup;
	///
	/// let mut command = Command::new("ls");
	/// if let Ok(mut child) = command.group_spawn() {
	///     child.wait().await.expect("command wasn't running");
	///     println!("Child has finished its execution!");
	/// } else {
	///     println!("ls command didn't start");
	/// }
	/// # }
	/// ```
	pub async fn wait(&mut self) -> Result<ExitStatus> {
		if let Some(es) = self.exitstatus {
			return Ok(es);
		}

		drop(self.imp.take_stdin());
		let status = self.imp.wait().await?;
		self.exitstatus = Some(status);
		Ok(status)
	}

	/// Attempts to collect the exit status of the child if it has already exited.
	///
	/// See [the Tokio documentation](Child::try_wait) for more.
	///
	/// # Examples
	///
	/// Basic usage:
	///
	/// ```no_run
	/// # #[tokio::main]
	/// # async fn main() {
	/// use tokio::process::Command;
	/// use command_group::AsyncCommandGroup;
	///
	/// let mut child = Command::new("ls").group_spawn().unwrap();
	///
	/// match child.try_wait() {
	///     Ok(Some(status)) => println!("exited with: {}", status),
	///     Ok(None) => {
	///         println!("status not ready yet, let's really wait");
	///         let res = child.wait().await;
	///         println!("result: {:?}", res);
	///     }
	///     Err(e) => println!("error attempting to wait: {}", e),
	/// }
	/// # }
	/// ```
	pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		if self.exitstatus.is_some() {
			return Ok(self.exitstatus);
		}

		match self.imp.try_wait()? {
			Some(es) => {
				self.exitstatus = Some(es);
				Ok(Some(es))
			}
			None => Ok(None),
		}
	}

	/// Simultaneously waits for the child to exit and collect all remaining output on the
	/// stdout/stderr handles, returning an `Output` instance.
	///
	/// See [the Tokio documentation](Child::wait_with_output) for more.
	///
	/// # Examples
	///
	/// Basic usage:
	///
	/// ```should_panic
	/// # #[tokio::main]
	/// # async fn main() {
	/// use std::process::Stdio;
	/// use tokio::process::Command;
	/// use command_group::AsyncCommandGroup;
	///
	/// let child = Command::new("/bin/cat")
	///     .arg("file.txt")
	///     .stdout(Stdio::piped())
	///     .group_spawn()
	///     .expect("failed to execute child");
	///
	/// let output = child
	///     .wait_with_output()
	///     .await
	///     .expect("failed to wait on child");
	///
	/// assert!(output.status.success());
	/// # }
	/// ```
	pub async fn wait_with_output(mut self) -> Result<Output> {
		drop(self.imp.take_stdin());

		let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
		match (self.imp.take_stdout(), self.imp.take_stderr()) {
			(None, None) => {}
			(Some(mut out), None) => {
				out.read_to_end(&mut stdout).await?;
			}
			(None, Some(mut err)) => {
				err.read_to_end(&mut stderr).await?;
			}
			(Some(mut out), Some(mut err)) => {
				// TODO: replace with futures crate usage
				// and drop macros feature from tokio
				tokio::try_join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr),)?;
			}
		}

		let status = self.imp.wait().await?;
		Ok(Output {
			status,
			stdout,
			stderr,
		})
	}
}

#[cfg(unix)]
impl crate::UnixChildExt for AsyncGroupChild {
	fn signal(&self, sig: Signal) -> Result<()> {
		self.imp.signal_imp(sig)
	}
}
