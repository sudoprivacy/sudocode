use std::{
	io::{Read, Result},
	mem,
	process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus},
};
use winapi::{
	shared::{
		basetsd::ULONG_PTR,
		minwindef::{DWORD, FALSE},
	},
	um::{
		handleapi::CloseHandle, ioapiset::GetQueuedCompletionStatus, jobapi2::TerminateJobObject,
		minwinbase::OVERLAPPED, winbase::INFINITE, winnt::HANDLE,
	},
};

use crate::winres::*;

pub(super) struct ChildImp {
	inner: Child,
	handles: JobPort,
}

impl ChildImp {
	pub fn new(inner: Child, job: HANDLE, completion_port: HANDLE) -> Self {
		Self {
			inner,
			handles: JobPort {
				job,
				completion_port,
			},
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
		// manually drop the completion port
		let its = mem::ManuallyDrop::new(self.handles);
		unsafe { CloseHandle(its.completion_port) };
		// we leave the job handle unclosed, otherwise the Child is useless
		// (as closing it will terminate the job)

		// extract the Child
		self.inner
	}

	pub fn group_is_empty(&self) -> Result<bool> {
		self.handles.is_empty()
	}

	pub fn kill(&mut self) -> Result<()> {
		res_bool(unsafe { TerminateJobObject(self.handles.job, 1) })
	}

	pub fn id(&self) -> u32 {
		self.inner.id()
	}

	fn wait_imp(&self, timeout: DWORD) -> Result<()> {
		let mut code: DWORD = 0;
		let mut key: ULONG_PTR = 0;
		let mut overlapped = mem::MaybeUninit::<OVERLAPPED>::uninit();
		let mut lp_overlapped = overlapped.as_mut_ptr();

		let result = unsafe {
			GetQueuedCompletionStatus(
				self.handles.completion_port,
				&mut code,
				&mut key,
				&mut lp_overlapped,
				timeout,
			)
		};

		// ignore timing out errors unless the timeout was specified to INFINITE
		// https://docs.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-getqueuedcompletionstatus
		if timeout != INFINITE && result == FALSE && lp_overlapped.is_null() {
			return Ok(());
		}

		res_bool(result)?;

		Ok(())
	}

	pub fn wait(&mut self) -> Result<ExitStatus> {
		self.wait_imp(INFINITE)?;
		self.inner.wait()
	}

	pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		self.wait_imp(0)?;
		self.inner.try_wait()
	}

	pub(super) fn read_both(
		mut out_r: ChildStdout,
		out_v: &mut Vec<u8>,
		mut err_r: ChildStderr,
		err_v: &mut Vec<u8>,
	) -> Result<()> {
		out_r.read_to_end(out_v)?;
		err_r.read_to_end(err_v)?;
		Ok(())
	}
}
