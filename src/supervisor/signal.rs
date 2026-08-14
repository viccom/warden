//! 跨平台进程信号与进程树管理。
//!
//! - **Windows**:`CREATE_NEW_PROCESS_GROUP` 启动(继承 console + 独立 pgid)
//!   + Job Object(`KILL_ON_JOB_CLOSE` 杀整棵树 + warden 崩溃保护)
//!   + `GenerateConsoleCtrlEvent(CTRL_C_EVENT)` 优雅停止(触发子进程 tokio ctrl_c)
//! - **Unix**:`process_group(0)` 让子进程自成新进程组 + `killpg` SIGTERM/SIGKILL

use std::io;

use tokio::process::Command;

#[cfg(windows)]
pub use windows_imp::JobGuard as JobTree;
#[cfg(unix)]
pub type JobTree = ();

/// spawn 前配置 Command,使子进程自成新进程组。
///
/// Windows:`CREATE_NEW_PROCESS_GROUP`(0x200)——子进程继承 console 但成为独立 pgid,
/// 使 `GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid)` 能精确投递而不广播误伤 warden。
/// Unix:`process_group(0)` 让子进程自成新进程组,便于 killpg。
pub fn prepare_command(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0000_0200);
    }
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
}

/// 创建进程树追踪对象(Windows=Job Object;Unix=无,靠 pgid)。
pub fn create_job_tree() -> io::Result<JobTree> {
    #[cfg(windows)]
    {
        windows_imp::create_job()
    }
    #[cfg(unix)]
    {
        Ok(())
    }
}

/// 把子进程纳入进程树追踪(Windows assign Job;Unix no-op)。
pub fn assign_to_job(job: &JobTree, child: &tokio::process::Child) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows_imp::assign_to_job(job, child)
    }
    #[cfg(unix)]
    {
        let _ = (job, child);
        Ok(())
    }
}

/// 发优雅停止信号(Windows CTRL_C_EVENT;Unix SIGTERM)。
pub fn send_graceful(pgid: u32) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows_imp::send_graceful(pgid)
    }
    #[cfg(unix)]
    {
        unix_imp::send_graceful(pgid)
    }
}

/// 强杀整棵进程树(Windows TerminateJobObject;Unix killpg SIGKILL)。
pub fn force_kill_tree(job: &JobTree, pgid: u32) {
    #[cfg(windows)]
    {
        windows_imp::force_kill_tree(job);
        let _ = pgid;
    }
    #[cfg(unix)]
    {
        unix_imp::force_kill_tree(pgid);
        let _ = job;
    }
}

#[cfg(windows)]
mod windows_imp {
    use std::io;

    use tokio::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        SetInformationJobObject, TerminateJobObject,
    };

    /// Job Object 句柄。Drop 时 CloseHandle 触发 KILL_ON_JOB_CLOSE(杀整棵进程树)。
    pub struct JobGuard(HANDLE);

    // HANDLE 是进程句柄,可跨线程使用(Windows 句柄本身线程安全)。
    // windows-sys 的 raw HANDLE 默认 !Send/!Sync,这里显式声明。
    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn create_job() -> io::Result<JobGuard> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(h);
                return Err(io::Error::last_os_error());
            }
            Ok(JobGuard(h))
        }
    }

    pub fn assign_to_job(job: &JobGuard, child: &Child) -> io::Result<()> {
        let raw = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("子进程无句柄"))?;
        unsafe {
            if AssignProcessToJobObject(job.0, raw as HANDLE) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn send_graceful(pgid: u32) -> io::Result<()> {
        unsafe {
            // 独立 process group 上 CTRL_C_EVENT 不投递(Windows quirk),
            // 只能用 CTRL_BREAK_EVENT 跨 group(要求子进程监听 ctrl_break)。
            if GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pgid) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn force_kill_tree(job: &JobGuard) {
        unsafe {
            TerminateJobObject(job.0, 1);
        }
    }
}

#[cfg(unix)]
mod unix_imp {
    use std::io;

    pub fn send_graceful(pgid: u32) -> io::Result<()> {
        unsafe {
            if libc::killpg(pgid as i32, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn force_kill_tree(pgid: u32) {
        unsafe {
            libc::killpg(pgid as i32, libc::SIGKILL);
        }
    }
}
