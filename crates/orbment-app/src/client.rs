use anyhow::Result;
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetStdHandle,
};

/// A GUI-subsystem process must attach explicitly when launched from a terminal.
/// Preserve redirected handles: attaching must never replace MCP's transport pipes.
fn attach_parent_console() {
    unsafe {
        let handles = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|kind| {
            (
                kind,
                GetStdHandle(kind)
                    .ok()
                    .filter(|handle| !handle.is_invalid()),
            )
        });
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        for (kind, handle) in handles {
            if let Some(handle) = handle {
                let _ = SetStdHandle(kind, handle);
            }
        }
    }
}

pub fn run_if_requested() -> Option<Result<()>> {
    let mut args = std::env::args().skip(1);
    let command = args.next()?;
    if command != "mcp" && command != "cli" {
        return None;
    }
    attach_parent_console();
    Some(if command == "mcp" {
        if args.next().is_some() {
            Err(anyhow::anyhow!("mcp does not accept arguments"))
        } else {
            orbment_mcp::run_stdio()
        }
    } else {
        orbment_mcp::run_cli(&args.collect::<Vec<_>>())
    })
}
