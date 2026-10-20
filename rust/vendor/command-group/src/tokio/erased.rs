use std::{
	io::Result,
	process::{ExitStatus, Output},
};

use super::AsyncGroupChild;
use tokio::process::Child;

/// Wrapper around a process child, be it grouped or ungrouped.
///
/// This is a helper which erases that a [`tokio::process::Child`] is a different type than an
/// [`AsyncGroupChild`]. It forwards to the corresponding method on the inner type.
#[derive(Debug)]
pub enum ErasedChild {
	/// A grouped process child.
	Grouped(AsyncGroupChild),

	/// An ungrouped process child.
	Ungrouped(Child),
}

impl ErasedChild {
	/// Returns the OS-assigned process (group) identifier.
	///
	/// - Grouped: [`AsyncGroupChild::id`]
	/// - Ungrouped: [`Child::id`]
	pub fn id(&mut self) -> Option<u32> {
		match self {
			Self::Grouped(c) => c.id(),
			Self::Ungrouped(c) => c.id(),
		}
	}

	/// Forces the child to exit.
	///
	/// - Grouped: [`AsyncGroupChild::kill`]
	/// - Ungrouped: [`Child::kill`]
	pub async fn kill(&mut self) -> Result<()> {
		match self {
			Self::Grouped(c) => c.kill().await,
			Self::Ungrouped(c) => c.kill().await,
		}
	}

	/// Attempts to force the child to exit, but does not wait for the request to take effect.
	///
	/// - Grouped: [`AsyncGroupChild::start_kill`]
	/// - Ungrouped: [`Child::start_kill`]
	pub fn start_kill(&mut self) -> Result<()> {
		match self {
			Self::Grouped(c) => c.start_kill(),
			Self::Ungrouped(c) => c.start_kill(),
		}
	}

	/// Attempts to collect the exit status of the child if it has already exited.
	///
	/// - Grouped: [`AsyncGroupChild::try_wait`]
	/// - Ungrouped: [`Child::try_wait`]
	pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		match self {
			Self::Grouped(c) => c.try_wait(),
			Self::Ungrouped(c) => c.try_wait(),
		}
	}

	/// Waits for the process to exit, and returns its exit status.
	///
	/// - Grouped: [`AsyncGroupChild::wait`]
	/// - Ungrouped: [`Child::wait`]
	pub async fn wait(&mut self) -> Result<ExitStatus> {
		match self {
			Self::Grouped(c) => c.wait().await,
			Self::Ungrouped(c) => c.wait().await,
		}
	}

	/// Waits for the process to exit, and returns its exit status.
	///
	/// - Grouped: [`AsyncGroupChild::wait_with_output`]
	/// - Ungrouped: [`Child::wait_with_output`]
	pub async fn wait_with_output(self) -> Result<Output> {
		match self {
			Self::Grouped(c) => c.wait_with_output().await,
			Self::Ungrouped(c) => c.wait_with_output().await,
		}
	}

	/// Sends a Unix signal to the process.
	///
	/// - Grouped: [`AsyncGroupChild::signal`]
	/// - Ungrouped: [`Child::signal`]
	#[cfg(unix)]
	pub fn signal(&self, sig: crate::Signal) -> Result<()> {
		use crate::UnixChildExt;

		match self {
			Self::Grouped(c) => c.signal(sig),
			Self::Ungrouped(c) => c.signal(sig),
		}
	}
}
