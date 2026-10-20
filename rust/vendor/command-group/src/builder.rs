//!

/// CommandGroupBuilder is a builder for a group of processes.
///
/// It is created via the `group` method on [`Command`](std::process::Command) or
/// [`AsyncCommand`](tokio::process::Command).
pub struct CommandGroupBuilder<'a, T> {
	pub(crate) command: &'a mut T,
	#[allow(dead_code)]
	pub(crate) kill_on_drop: bool,
	#[allow(dead_code)]
	pub(crate) creation_flags: u32,
}

impl<'a, T> CommandGroupBuilder<'a, T> {
	pub(crate) fn new(command: &'a mut T) -> Self {
		Self {
			command,
			kill_on_drop: false,
			creation_flags: 0,
		}
	}

	/// See [`tokio::process::Command::kill_on_drop`].
	#[cfg(any(windows, feature = "with-tokio"))]
	pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
		self.kill_on_drop = kill_on_drop;
		self
	}

	/// Set the creation flags for the process.
	#[cfg(windows)]
	pub fn creation_flags(&mut self, creation_flags: u32) -> &mut Self {
		self.creation_flags = creation_flags;
		self
	}
}
