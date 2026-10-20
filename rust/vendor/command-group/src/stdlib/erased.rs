use std::{
	io::Result,
	process::{Child, ExitStatus, Output},
};

use super::GroupChild;

/// Wrapper around a process child, be it grouped or ungrouped.
///
/// This is a helper which erases that a [`std::process::Child`] is a different type than a
/// [`GroupChild`]. It forwards to the corresponding method on the inner type.
#[derive(Debug)]
pub enum ErasedChild {
	/// A grouped process child.
	Grouped(GroupChild),

	/// An ungrouped process child.
	Ungrouped(Child),
}

impl ErasedChild {
	/// Returns the OS-assigned process (group) identifier.
	///
	/// - Grouped: [`GroupChild::id`]
	/// - Ungrouped: [`Child::id`]
	pub fn id(&mut self) -> u32 {
		match self {
			Self::Grouped(c) => c.id(),
			Self::Ungrouped(c) => c.id(),
		}
	}

	/// Forces the child to exit.
	///
	/// - Grouped: [`GroupChild::kill`]
	/// - Ungrouped: [`Child::kill`]
	pub fn kill(&mut self) -> Result<()> {
		match self {
			Self::Grouped(c) => c.kill(),
			Self::Ungrouped(c) => c.kill(),
		}
	}

	/// Attempts to collect the exit status of the child if it has already exited.
	///
	/// - Grouped: [`GroupChild::try_wait`]
	/// - Ungrouped: [`Child::try_wait`]
	pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		match self {
			Self::Grouped(c) => c.try_wait(),
			Self::Ungrouped(c) => c.try_wait(),
		}
	}

	/// Waits for the process to exit, and returns its exit status.
	///
	/// - Grouped: [`GroupChild::wait`]
	/// - Ungrouped: [`Child::wait`]
	pub fn wait(&mut self) -> Result<ExitStatus> {
		match self {
			Self::Grouped(c) => c.wait(),
			Self::Ungrouped(c) => c.wait(),
		}
	}

	/// Waits for the process to exit, and returns its exit status.
	///
	/// - Grouped: [`GroupChild::wait_with_output`]
	/// - Ungrouped: [`Child::wait_with_output`]
	pub fn wait_with_output(self) -> Result<Output> {
		match self {
			Self::Grouped(c) => c.wait_with_output(),
			Self::Ungrouped(c) => c.wait_with_output(),
		}
	}

	/// Sends a Unix signal to the process.
	///
	/// - Grouped: [`GroupChild::signal`]
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
