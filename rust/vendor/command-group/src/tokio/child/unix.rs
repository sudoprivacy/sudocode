use std::{
	convert::TryInto,
	io::{Error, Result},
	ops::ControlFlow,
	os::unix::process::ExitStatusExt,
	process::ExitStatus,
};

use nix::{
	errno::Errno,
	libc,
	sys::{
		signal::{killpg, Signal},
		wait::WaitPidFlag,
	},
	unistd::Pid,
};
use tokio::{
	process::{Child, ChildStderr, ChildStdin, ChildStdout},
	task::spawn_blocking,
};

pub(super) struct ChildImp {
	pgid: Pid,
	inner: Child,
}

impl ChildImp {
	pub(super) fn new(inner: Child) -> Self {
		let pid = inner
			.id()
			.expect("Command was reaped before we could read its PID")
			.try_into()
			.expect("Command PID > i32::MAX");
		Self {
			pgid: Pid::from_raw(pid),
			inner,
		}
	}

	pub(super) fn take_stdin(&mut self) -> Option<ChildStdin> {
		self.inner.stdin.take()
	}

	pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
		self.inner.stdout.take()
	}

	pub(super) fn take_stderr(&mut self) -> Option<ChildStderr> {
		self.inner.stderr.take()
	}

	pub fn inner(&mut self) -> &mut Child {
		&mut self.inner
	}

	pub fn into_inner(self) -> Child {
		self.inner
	}

	pub(super) fn signal_imp(&self, sig: Signal) -> Result<()> {
		killpg(self.pgid, sig).map_err(Error::from)
	}

	pub fn start_kill(&mut self) -> Result<()> {
		self.signal_imp(Signal::SIGKILL)
	}

	pub fn id(&self) -> Option<u32> {
		self.inner.id()
	}

	fn wait_imp(pgid: i32, flag: WaitPidFlag) -> Result<ControlFlow<Option<ExitStatus>>> {
		// Wait for processes in a loop until every process in this
		// process group has exited (this ensures that we reap any
		// zombies that may have been created if the parent exited after
		// spawning children, but didn't wait for those children to
		// exit).
		let mut parent_exit_status: Option<ExitStatus> = None;
		loop {
			// we can't use the safe wrapper directly because it doesn't
			// return the raw status, and we need it to convert to the
			// std's ExitStatus.
			let mut status: i32 = 0;
			match unsafe { libc::waitpid(-pgid, &mut status as *mut libc::c_int, flag.bits()) } {
				0 => {
					// Zero should only happen if WNOHANG was passed in,
					// and means that no processes have yet to exit.
					return Ok(ControlFlow::Continue(()));
				}
				-1 => {
					match Errno::last() {
						Errno::ECHILD => {
							// No more children to reap; this is a
							// graceful exit.
							return Ok(ControlFlow::Break(parent_exit_status));
						}
						errno => {
							return Err(Error::from(errno));
						}
					}
				}
				pid => {
					// *A* process exited. Was it the parent process
					// that we started? If so, collect the exit signal,
					// otherwise we reaped a zombie process and should
					// continue in the loop.
					if pgid == pid {
						parent_exit_status = Some(ExitStatus::from_raw(status));
					} else {
						// Reaped a zombie child; keep looping.
					}
				}
			};
		}
	}

	pub async fn wait(&mut self) -> Result<ExitStatus> {
		const MAX_RETRY_ATTEMPT: usize = 10;

		// Always wait for parent to exit first.
		//
		// It's likely that all its children has already exited and reaped by
		// the time the parent exits.
		let status = self.inner.wait().await?;

		let pgid = self.pgid.as_raw();

		// Try reaping all children, if there are some that are still alive after
		// several attempts, then spawn a blocking task to reap them.
		for retry_attempt in 1..=MAX_RETRY_ATTEMPT {
			if Self::wait_imp(pgid, WaitPidFlag::WNOHANG)?.is_break() {
				break;
			} else if retry_attempt == MAX_RETRY_ATTEMPT {
				spawn_blocking(move || Self::wait_imp(pgid, WaitPidFlag::empty())).await??;
			}
		}

		Ok(status)
	}

	pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		match Self::wait_imp(self.pgid.as_raw(), WaitPidFlag::WNOHANG)? {
			ControlFlow::Break(res) => Ok(res),
			ControlFlow::Continue(()) => self.inner.try_wait(),
		}
	}
}

impl crate::UnixChildExt for ChildImp {
	fn signal(&self, sig: Signal) -> Result<()> {
		self.signal_imp(sig)
	}
}
