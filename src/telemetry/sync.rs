// When to copy evidence to MongoDB. Data reaches the database at the moment it is complete: a tool
// call when it ends (post-tool hook), a session when it ends (stop and session-end hooks), a
// governance or other Crane command when it finishes, and everything else periodically (any Crane
// run or hook starts a sync when the last attempt is older than the configured interval, and the
// dashboard syncs on a timer). A sync runs as a detached 'crane agent sync' process with its output
// discarded, so a hook or command never waits for the network and a slow or unreachable database
// never delays or weakens enforcement.

use std::process::{Command, Stdio};

use serde_json::Value;

use super::Stores;
use crate::governance::workspace::Workspace;
use crate::integrations;
use crate::platform::now_millis;

/** Default interval of the periodic sync, in seconds */
pub(crate) const DEFAULT_INTERVAL_SECONDS: u64 = 60;

/** Report whether this build can sync and MongoDB is configured (the mongodb integration, or the
 * CRANE_MONGODB_URI fallback for CI)
 * Input
    - workspace: &Workspace - repository
 * Output
    - bool
*/
pub(crate) fn configured(workspace: &Workspace) -> bool {
    cfg!(feature = "mongodb")
        && (integrations::load(workspace, "mongodb").is_some()
            || std::env::var("CRANE_MONGODB_URI").is_ok_and(|uri| !uri.is_empty()))
}

/** Return the periodic sync interval
 * Input
    - workspace: &Workspace - repository
 * Output
    - u64 seconds
*/
pub(crate) fn interval_seconds(workspace: &Workspace) -> u64 {
    integrations::load(workspace, "mongodb")
        .and_then(|integration| {
            integration
                .settings
                .get("sync_seconds")
                .and_then(Value::as_u64)
        })
        .unwrap_or(DEFAULT_INTERVAL_SECONDS)
        .max(5)
}

/** Start a detached sync now, when MongoDB is configured
 * Input
    - workspace: &Workspace - repository
 * Output
    - None (failures to start are ignored; the next trigger retries)
*/
pub(crate) fn trigger(workspace: &Workspace) {
    if !configured(workspace) {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .args(["agent", "sync"])
        .current_dir(&workspace.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        disinherit_standard_handles();
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console, not stopped with the parent
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let _ = command.spawn();
}

/** Stop this process's standard handles from being inherited by the detached sync. Windows passes
 * every inheritable handle to a child, so without this the sync would keep the agent host's hook
 * pipes open and the host would wait for the sync to finish. Standard handles a later child is
 * explicitly given are duplicated for it by the standard library, so nothing else is affected.
 * Input
    - None
 * Output
    - None
*/
#[cfg(windows)]
fn disinherit_standard_handles() {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x1;
    // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE are -10, -11, -12 as DWORD
    for kind in [-10i32, -11, -12] {
        // SAFETY: GetStdHandle takes no pointers and returns a handle owned by this process (or
        // null or INVALID_HANDLE_VALUE, which are skipped); SetHandleInformation only changes that
        // handle's inheritance flag and never closes or invalidates it.
        unsafe {
            let handle = GetStdHandle(kind as u32);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/** Start a detached sync when the last attempt is older than the interval
 * Input
    - workspace: &Workspace - repository
    - stores: &Stores - runtime stores
 * Output
    - None
*/
pub(crate) fn trigger_if_due(workspace: &Workspace, stores: &Stores) {
    if !configured(workspace) {
        return;
    }
    let last = std::fs::read_to_string(stores.directory.join("mongodb-state.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|state| state["last_attempt_at"].as_u64())
        .unwrap_or(0);
    if now_millis().saturating_sub(last) >= interval_seconds(workspace) * 1000 {
        trigger(workspace);
    }
}
