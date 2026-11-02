//! Contain tool descendants. Windows starts suspended so containment precedes tool execution.

use std::io;
use std::ops::{Deref, DerefMut};
use std::process::{Child, Command};

pub(crate) struct ToolProcess {
	child: Child,
	#[cfg(windows)]
	job: windows::Job,
}

impl ToolProcess {
	pub fn spawn(command: &mut Command) -> io::Result<Self> {
		#[cfg(windows)]
		{
			use std::os::windows::process::CommandExt;
			use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
			let job = windows::Job::new()?;
			command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
			let mut child = command.spawn()?;
			if let Err(error) = job.attach_and_resume(&child) {
				let _ = child.kill();
				let _ = child.wait();
				return Err(error);
			}
			Ok(Self { child, job })
		}
		#[cfg(not(windows))]
		{
			Ok(Self {
				child: command.spawn()?,
			})
		}
	}

	pub fn has_helpers(&self) -> io::Result<bool> {
		#[cfg(unix)]
		{
			let pid = libc::pid_t::try_from(self.child.id()).map_err(io::Error::other)?;
			if unsafe { libc::kill(-pid, 0) } == 0 {
				return Ok(true);
			}
			let error = io::Error::last_os_error();
			if error.raw_os_error() == Some(libc::ESRCH) {
				Ok(false)
			} else {
				Err(error)
			}
		}
		#[cfg(windows)]
		{
			self.job.has_processes()
		}
		#[cfg(not(any(unix, windows)))]
		{
			Err(io::Error::new(
				io::ErrorKind::Unsupported,
				"tool process containment unavailable",
			))
		}
	}
}

impl Deref for ToolProcess {
	type Target = Child;
	fn deref(&self) -> &Child {
		&self.child
	}
}

impl DerefMut for ToolProcess {
	fn deref_mut(&mut self) -> &mut Child {
		&mut self.child
	}
}

#[cfg(windows)]
mod windows {
	use std::io;
	use std::mem::{size_of, size_of_val, zeroed};
	use std::os::windows::io::AsRawHandle;
	use std::process::Child;

	use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
	use windows_sys::Win32::System::Diagnostics::ToolHelp::{
		CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
	};
	use windows_sys::Win32::System::JobObjects::{
		AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
		JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
		JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject,
		SetInformationJobObject,
	};
	use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

	struct Handle(HANDLE);
	impl Drop for Handle {
		fn drop(&mut self) {
			unsafe {
				CloseHandle(self.0);
			}
		}
	}

	pub(super) struct Job(Handle);

	impl Job {
		pub fn new() -> io::Result<Self> {
			let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
			if handle.is_null() {
				return Err(io::Error::last_os_error());
			}
			let job = Self(Handle(handle));
			let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
			limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
			let result = unsafe {
				SetInformationJobObject(
					job.0.0,
					JobObjectExtendedLimitInformation,
					std::ptr::from_ref(&limits).cast(),
					size_of_val(&limits) as u32,
				)
			};
			if result == 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(job)
		}

		pub fn attach_and_resume(&self, child: &Child) -> io::Result<()> {
			if unsafe { AssignProcessToJobObject(self.0.0, child.as_raw_handle() as HANDLE) } == 0 {
				return Err(io::Error::last_os_error());
			}
			let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
			if snapshot == INVALID_HANDLE_VALUE {
				return Err(io::Error::last_os_error());
			}
			let snapshot = Handle(snapshot);
			let mut entry: THREADENTRY32 = unsafe { zeroed() };
			entry.dwSize = size_of::<THREADENTRY32>() as u32;
			let mut found = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
			while found {
				if entry.th32OwnerProcessID == child.id() {
					let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
					if thread.is_null() {
						return Err(io::Error::last_os_error());
					}
					let thread = Handle(thread);
					if unsafe { ResumeThread(thread.0) } == u32::MAX {
						return Err(io::Error::last_os_error());
					}
					return Ok(());
				}
				found = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
			}
			Err(io::Error::other("suspended tool thread could not be found"))
		}

		pub fn has_processes(&self) -> io::Result<bool> {
			let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
			let result = unsafe {
				QueryInformationJobObject(
					self.0.0,
					JobObjectBasicAccountingInformation,
					std::ptr::from_mut(&mut info).cast(),
					size_of_val(&info) as u32,
					std::ptr::null_mut(),
				)
			};
			if result == 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(info.ActiveProcesses != 0)
		}
	}
}
