//! Windows Job Object 进程树终止（TODO「超时机制问题 · 问题 1」的彻底方案）。
//!
//! 背景：`cmd /C mvn package` 起的 `java.exe` / `node.exe` 不是直接子进程，
//! `Child::kill()` 只杀最外层 cmd.exe，Windows 上 kill 不级联——超时 / 取消后
//! 构建进程变成孤儿继续跑，占着 target/ 与文件锁。
//!
//! 方案：spawn 后立即把子进程挂进一个 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! 的 Job Object。句柄关闭（Drop = 超时丢弃 future）或显式 `terminate()`
//! （取消路径）时整棵进程树一起终止。子进程之后 spawn 的孙进程自动同属 job。
//!
//! 非 Windows 目标为空实现（本项目主链路面向 Windows 发版机）。

/// 子进程的进程树守卫：创建即挂 job，Drop 时整树终止（KILL_ON_JOB_CLOSE）。
/// 挂载失败（句柄已退出 / 嵌套 job 限制）降级为无守卫——直接 kill 兜底仍有效。
#[cfg(windows)]
mod win {
    pub use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    };
    pub use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
}

#[cfg(windows)]
pub struct ProcTreeGuard {
    /// Job Object 句柄（存 isize 以满足 Send；0 = 未挂载）
    handle: isize,
}

#[cfg(windows)]
impl ProcTreeGuard {
    pub fn attach(child: &tokio::process::Child) -> Self {
        use win::CreateJobObjectW;
        use win::{AssignProcessToJobObject, SetInformationJobObject};
        use win::{JobObjectExtendedLimitInformation, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
        use win::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;

        let handle =
            unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Self { handle: 0 };
        }
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *mut _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
        }
        if let Some(proc_handle) = child.raw_handle() {
            // tokio 返回 std::ffi::c_void 指针，转 winapi 的 HANDLE
            unsafe { AssignProcessToJobObject(handle, proc_handle as _) };
        }
        Self { handle: handle as isize }
    }

    /// 取消路径：立即终止整棵进程树（比逐个 kill 快且无漏）
    pub fn terminate(&self) {
        if self.handle != 0 {
            unsafe { win::TerminateJobObject(self.handle as _, 1) };
        }
    }
}

#[cfg(windows)]
impl Drop for ProcTreeGuard {
    fn drop(&mut self) {
        if self.handle != 0 {
            // KILL_ON_JOB_CLOSE：最后一个句柄关闭 → 整棵树终止（超时丢弃 future 的路径）
            unsafe { winapi::um::handleapi::CloseHandle(self.handle as _) };
        }
    }
}

#[cfg(not(windows))]
pub struct ProcTreeGuard;

#[cfg(not(windows))]
impl ProcTreeGuard {
    pub fn attach(_child: &tokio::process::Child) -> Self {
        Self
    }

    pub fn terminate(&self) {}
}
