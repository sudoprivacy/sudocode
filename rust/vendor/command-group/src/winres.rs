use std::{
	convert::TryInto,
	io::{Error, ErrorKind, Result},
	mem,
	os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
	ptr,
};
use winapi::{
	shared::{
		minwindef::{BOOL, DWORD, FALSE, LPVOID},
		winerror::ERROR_NO_MORE_FILES,
	},
	um::{
		handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
		ioapiset::CreateIoCompletionPort,
		jobapi2::{
			AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
			SetInformationJobObject,
		},
		processthreadsapi::{GetProcessId, OpenThread, ResumeThread},
		tlhelp32::{
			CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
		},
		winnt::{
			JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation,
			JobObjectExtendedLimitInformation, HANDLE, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
			JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
			JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
		},
	},
};

#[derive(Clone)]
pub(crate) struct JobPort {
	pub job: HANDLE,
	pub completion_port: HANDLE,
}

impl JobPort {
	pub(crate) fn is_empty(&self) -> Result<bool> {
		let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
		// The owned Job handle covers descendants assigned before the process
		// was resumed. Leader exit and arbitrary completion-port events do not.
		res_bool(unsafe {
			QueryInformationJobObject(
				self.job,
				JobObjectBasicAccountingInformation,
				&mut info as *mut _ as LPVOID,
				mem::size_of_val(&info)
					.try_into()
					.expect("cannot safely cast to DWORD"),
				ptr::null_mut(),
			)
		})?;
		Ok(info.ActiveProcesses == 0)
	}
}

impl Drop for JobPort {
	fn drop(&mut self) {
		unsafe { CloseHandle(self.job) };
		unsafe { CloseHandle(self.completion_port) };
	}
}

unsafe impl Send for JobPort {}
unsafe impl Sync for JobPort {}

#[derive(Copy, Clone)]
#[repr(transparent)]
pub(crate) struct ThreadSafeRawHandle(pub HANDLE);

unsafe impl Send for ThreadSafeRawHandle {}
unsafe impl Sync for ThreadSafeRawHandle {}

pub(crate) fn res_null(handle: HANDLE) -> Result<HANDLE> {
	if handle.is_null() {
		Err(Error::last_os_error())
	} else {
		Ok(handle)
	}
}

pub(crate) fn res_bool(ret: BOOL) -> Result<()> {
	if ret == FALSE {
		Err(Error::last_os_error())
	} else {
		Ok(())
	}
}

pub(crate) fn res_neg(ret: DWORD) -> Result<DWORD> {
	if ret == DWORD::MAX {
		Err(Error::last_os_error())
	} else {
		Ok(ret)
	}
}

pub(crate) fn job_object(kill_on_drop: bool) -> Result<(HANDLE, HANDLE)> {
	let job = res_null(unsafe { CreateJobObjectW(ptr::null_mut(), ptr::null()) })?;

	let completion_port =
		res_null(unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) })?;

	let mut associate_completion = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
		CompletionKey: job,
		CompletionPort: completion_port,
	};

	res_bool(unsafe {
		SetInformationJobObject(
			job,
			JobObjectAssociateCompletionPortInformation,
			&mut associate_completion as *mut _ as LPVOID,
			mem::size_of_val(&associate_completion)
				.try_into()
				.expect("cannot safely cast to DWORD"),
		)
	})?;

	let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();

	if kill_on_drop {
		info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
	}

	res_bool(unsafe {
		SetInformationJobObject(
			job,
			JobObjectExtendedLimitInformation,
			&mut info as *mut _ as LPVOID,
			mem::size_of_val(&info)
				.try_into()
				.expect("cannot safely cast to DWORD"),
		)
	})?;

	Ok((job, completion_port))
}

// This is pretty terrible, but it's either this or we re-implement all of Rust's std::process just
// to get at PROCESS_INFORMATION!
fn resume_threads(child_process: HANDLE) -> Result<()> {
	let child_id = unsafe { GetProcessId(child_process) };
	if child_id == 0 {
		return Err(Error::last_os_error());
	}

	let h = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
	if h == INVALID_HANDLE_VALUE {
		return Err(Error::last_os_error());
	}
	// Fresh native handles have one owner, including every early error return.
	let snapshot = unsafe { OwnedHandle::from_raw_handle(h.cast()) };
	let mut entry = THREADENTRY32 {
		dwSize: 28,
		cntUsage: 0,
		th32ThreadID: 0,
		th32OwnerProcessID: 0,
		tpBasePri: 0,
		tpDeltaPri: 0,
		dwFlags: 0,
	};

	res_bool(unsafe { Thread32First(snapshot.as_raw_handle().cast(), &mut entry) })?;
	let mut found_child_thread = false;
	loop {
		if entry.th32OwnerProcessID == child_id {
			let thread_handle = res_null(unsafe { OpenThread(0x0002, 0, entry.th32ThreadID) })?;
			let thread = unsafe { OwnedHandle::from_raw_handle(thread_handle.cast()) };
			res_neg(unsafe { ResumeThread(thread.as_raw_handle().cast()) })?;
			found_child_thread = true;
		}

		if unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &mut entry) } == FALSE {
			let error = Error::last_os_error();
			if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
				return Err(error);
			}
			break;
		}
	}

	if !found_child_thread {
		return Err(Error::new(
			ErrorKind::NotFound,
			"the suspended child had no thread in the native snapshot",
		));
	}
	Ok(())
}

pub(crate) fn assign_child(handle: RawHandle, job: HANDLE) -> Result<()> {
	let handle = handle as _;
	res_bool(unsafe { AssignProcessToJobObject(job, handle) })?;
	resume_threads(handle)?;
	Ok(())
}
