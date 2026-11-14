use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStringExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, RPC_E_CHANGED_MODE, WAIT_OBJECT_0},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
                REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
                RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
            SystemInformation::GetSystemDirectoryW,
            TaskScheduler::{IExecAction, ITaskService, TaskScheduler},
            Threading::{CREATE_NO_WINDOW, GetExitCodeProcess, INFINITE, WaitForSingleObject},
            Variant::VARIANT,
        },
    },
    core::{BSTR, Interface, w},
};

use crate::single_instance::{command_line, is_elevated, run_elevated};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct AutostartStatus {
    pub enabled: bool,
    pub path_matches: bool,
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result == RPC_E_CHANGED_MODE {
            // The caller already owns an MTA; Task Scheduler supports both apartments.
            return Ok(Self(false));
        }
        result
            .ok()
            .context("Initialize Task Scheduler COM apartment")?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn connect() -> Result<ITaskService> {
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
            .context("Create Task Scheduler service")?;
    let empty = VARIANT::default();
    unsafe { service.Connect(&empty, &empty, &empty, &empty) }
        .context("Connect to local Task Scheduler")?;
    Ok(service)
}

fn identity(service: &ITaskService) -> Result<(String, String)> {
    let user = unsafe { service.ConnectedUser()? }.to_string();
    let domain = unsafe { service.ConnectedDomain()? }.to_string();
    ensure!(
        !user.is_empty(),
        "Task Scheduler returned an empty user identity"
    );
    let account = if domain.is_empty() {
        user.clone()
    } else {
        format!("{domain}\\{user}")
    };
    // Separate tasks prevent one user's portable install from replacing another's.
    let name = format!("Orbment-{}", account.replace('\\', "-"));
    Ok((account, name))
}

pub fn status() -> Result<AutostartStatus> {
    resolve_status(task_status(), registry_status)
}

fn resolve_status(
    task: Result<AutostartStatus>,
    read_registry: impl FnOnce() -> Result<AutostartStatus>,
) -> Result<AutostartStatus> {
    let status = task?;
    if status.enabled {
        Ok(status)
    } else {
        read_registry()
    }
}

fn task_status() -> Result<AutostartStatus> {
    let _apartment = ComApartment::enter()?;
    let service = connect()?;
    let (_, name) = identity(&service)?;
    let folder = unsafe { service.GetFolder(&BSTR::from("\\"))? };
    let task = match unsafe { folder.GetTask(&BSTR::from(name)) } {
        Ok(task) => task,
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
            return Ok(AutostartStatus {
                enabled: false,
                path_matches: false,
            });
        }
        Err(error) => return Err(error).context("Read Orbment login task"),
    };
    let enabled = unsafe { task.Enabled()? }.as_bool();
    let actions = unsafe { task.Definition()?.Actions()? };
    let mut count = 0;
    unsafe {
        actions.Count(&mut count)?;
    }
    let path_matches = if count == 1 {
        match unsafe { actions.get_Item(1)? }.cast::<IExecAction>() {
            Ok(action) => {
                let mut path = BSTR::new();
                unsafe {
                    action.Path(&mut path)?;
                }
                executable_matches(Path::new(&OsString::from_wide(&path)))?
            }
            Err(_) => false,
        }
    } else {
        false
    };
    Ok(AutostartStatus {
        enabled,
        path_matches,
    })
}

fn executable_matches(task_path: &Path) -> Result<bool> {
    if !task_path.is_absolute() {
        return Ok(false);
    }
    let current =
        fs::canonicalize(std::env::current_exe()?).context("Resolve current executable path")?;
    match fs::canonicalize(task_path) {
        Ok(path) => Ok(path == current),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("Resolve login task executable path"),
    }
}

pub fn set_enabled(enabled: bool, data_dir: &Path) -> Result<()> {
    if enabled {
        match set_task_enabled(true, data_dir) {
            Ok(()) => remove_registry_entry(),
            Err(task_error) => set_registry_entry(data_dir)
                .with_context(|| format!("Task Scheduler also failed: {task_error:#}")),
        }
    } else {
        // Attempt both removals even if the scheduler is unavailable.
        let task_result = set_task_enabled(false, data_dir);
        let registry_result = remove_registry_entry();
        registry_result?;
        finish_disable(task_result, status)
    }
}

fn finish_disable(
    task_result: Result<()>,
    read_status: impl FnOnce() -> Result<AutostartStatus>,
) -> Result<()> {
    match task_result {
        Ok(()) => Ok(()),
        Err(error) => {
            if !read_status()
                .with_context(|| format!("Deleting login task also failed: {error:#}"))?
                .enabled
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn set_task_enabled(enabled: bool, data_dir: &Path) -> Result<()> {
    let _apartment = ComApartment::enter()?;
    let service = connect()?;
    let (account, name) = identity(&service)?;
    if !enabled {
        let folder = unsafe { service.GetFolder(&BSTR::from("\\"))? };
        match unsafe { folder.GetTask(&BSTR::from(name.as_str())) } {
            Ok(_) => {}
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(()),
            Err(error) => return Err(error).context("Read login task before deletion"),
        }
        return run_schtasks(&["/Delete".into(), "/TN".into(), name.into(), "/F".into()]);
    }

    let executable = std::env::current_exe().context("Locate Orbment executable")?;
    ensure!(
        executable.is_absolute() && executable.is_file(),
        "Orbment executable path is invalid"
    );
    let directory = executable
        .parent()
        .context("Orbment executable has no parent directory")?;
    let arguments = data_dir_arguments(data_dir)?;
    let parameters = String::from_utf16(&command_line(&arguments))
        .context("Data directory arguments are not valid Unicode")?;
    let executable_text = executable
        .to_str()
        .context("Executable path is not valid Unicode")?;
    let directory_text = directory
        .to_str()
        .context("Executable directory is not valid Unicode")?;
    let xml = login_task_xml(&account, executable_text, &parameters, directory_text);
    let temporary = TemporaryXml::create(&xml)?;
    run_schtasks(&[
        "/Create".into(),
        "/TN".into(),
        name.into(),
        "/XML".into(),
        temporary.0.as_os_str().to_owned(),
        "/F".into(),
    ])
}

fn login_task_xml(account: &str, executable: &str, arguments: &str, directory: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
        <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\
        <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId><Delay>PT3S</Delay></LogonTrigger></Triggers>\
        <Principals><Principal id=\"Author\"><UserId>{user}</UserId>\
        <LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>\
        <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\
        <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\
        <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\
        <StartWhenAvailable>true</StartWhenAvailable><Enabled>true</Enabled>\
        <ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>\
        <Actions Context=\"Author\"><Exec><Command>{executable}</Command>\
        <Arguments>{arguments}</Arguments><WorkingDirectory>{directory}</WorkingDirectory>\
        </Exec></Actions></Task>",
        user = xml_escape(account),
        executable = xml_escape(executable),
        arguments = xml_escape(arguments),
        directory = xml_escape(directory),
    )
}

fn data_dir_arguments(data_dir: &Path) -> Result<[OsString; 2]> {
    // A login task has a different working directory from the original launch.
    let path = std::path::absolute(data_dir).context("Resolve login task data directory")?;
    Ok(["--data-dir".into(), path.into_os_string()])
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

struct TemporaryXml(PathBuf);

impl TemporaryXml {
    fn create(xml: &str) -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path =
            std::env::temp_dir().join(format!("orbment-task-{}-{nonce}.xml", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("Create temporary login task definition")?;
        let temporary = Self(path);
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(xml.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        file.write_all(&bytes)
            .context("Write login task definition")?;
        file.sync_all().context("Flush login task definition")?;
        Ok(temporary)
    }
}

impl Drop for TemporaryXml {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            tracing::warn!(%error, path = %self.0.display(), "Remove temporary login task definition");
        }
    }
}

fn run_schtasks(arguments: &[OsString]) -> Result<()> {
    let mut system_directory = vec![0; 32768];
    let length = unsafe { GetSystemDirectoryW(Some(&mut system_directory)) } as usize;
    ensure!(
        length > 0 && length < system_directory.len(),
        "Read Windows system directory"
    );
    let executable =
        PathBuf::from(OsString::from_wide(&system_directory[..length])).join("schtasks.exe");
    if is_elevated()? {
        let output = Command::new(&executable)
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW.0)
            .output()
            .context("Run Windows Task Scheduler command")?;
        ensure!(
            output.status.success(),
            "Task Scheduler command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        let process =
            run_elevated(&executable, arguments).context("Authorize login task change")?;
        let wait = unsafe { WaitForSingleObject(process.0, INFINITE) };
        if wait != WAIT_OBJECT_0 {
            bail!("Wait for authorized Task Scheduler command: {wait:?}");
        }
        let mut exit_code = 0;
        unsafe {
            GetExitCodeProcess(process.0, &mut exit_code)?;
        }
        ensure!(
            exit_code == 0,
            "Authorized Task Scheduler command failed with exit code {exit_code}"
        );
    }
    Ok(())
}

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open_run_key() -> Result<Option<RegistryKey>> {
    let mut key = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            Some(0),
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    result.ok().context("Open per-user startup registry key")?;
    Ok(Some(RegistryKey(key)))
}

fn remove_registry_entry() -> Result<()> {
    if let Some(key) = open_run_key()? {
        let result = unsafe { RegDeleteValueW(key.0, w!("Orbment")) };
        if result != ERROR_FILE_NOT_FOUND {
            result.ok().context("Remove per-user startup entry")?;
        }
    }
    Ok(())
}

fn set_registry_entry(data_dir: &Path) -> Result<()> {
    let executable = std::env::current_exe().context("Locate Orbment executable")?;
    let arguments = data_dir_arguments(data_dir)?;
    let mut value = command_line(&[
        executable.into_os_string(),
        arguments[0].clone(),
        arguments[1].clone(),
    ]);
    value.push(0);
    let bytes: Vec<u8> = value.into_iter().flat_map(u16::to_le_bytes).collect();
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()
        .context("Create per-user startup registry key")?;
    }
    let key = RegistryKey(key);
    unsafe { RegSetValueExW(key.0, w!("Orbment"), Some(0), REG_SZ, Some(&bytes)) }
        .ok()
        .context("Write per-user startup entry")
}

fn registry_status() -> Result<AutostartStatus> {
    let disabled = AutostartStatus {
        enabled: false,
        path_matches: false,
    };
    let Some(key) = open_run_key()? else {
        return Ok(disabled);
    };
    let mut kind = REG_VALUE_TYPE::default();
    let mut bytes = vec![0u8; 32768 * 2];
    let mut length = bytes.len() as u32;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            w!("Orbment"),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut length),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(disabled);
    }
    result.ok().context("Read per-user startup entry")?;
    ensure!(
        kind == REG_SZ && length.is_multiple_of(2),
        "Invalid per-user startup entry"
    );
    let units: Vec<u16> = bytes[..length as usize]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| u16::from_le_bytes(*bytes))
        .take_while(|unit| *unit != 0)
        .collect();
    let text = String::from_utf16(&units).context("Invalid startup command Unicode")?;
    let text = text.trim();
    let executable = if let Some(quoted) = text.strip_prefix('"') {
        quoted.split_once('"').map(|(path, _)| path)
    } else {
        text.split_whitespace().next()
    };
    Ok(AutostartStatus {
        enabled: !text.is_empty(),
        path_matches: match executable {
            Some(path) => executable_matches(Path::new(path))?,
            None => false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_task_uses_delayed_elevated_interactive_logon() {
        let xml = login_task_xml(
            r"PC\user&name",
            r"C:\Orbment\orbment.exe",
            r#"--data-dir "C:\user data""#,
            r"C:\Orbment",
        );
        assert!(xml.contains(
            r"<LogonTrigger><Enabled>true</Enabled><UserId>PC\user&amp;name</UserId><Delay>PT3S</Delay></LogonTrigger>"
        ));
        assert!(xml.contains(
            "<LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel>"
        ));
        assert!(xml.contains(r"<Command>C:\Orbment\orbment.exe</Command>"));
        assert!(xml.contains(r"<Arguments>--data-dir &quot;C:\user data&quot;</Arguments>"));
        assert!(xml.contains(r"<WorkingDirectory>C:\Orbment</WorkingDirectory>"));
    }

    #[test]
    fn verified_disabled_status_succeeds_despite_task_deletion_failure() {
        assert!(
            finish_disable(Err(anyhow::anyhow!("access denied")), || {
                Ok(AutostartStatus {
                    enabled: false,
                    path_matches: false,
                })
            })
            .is_ok()
        );
        assert!(
            finish_disable(Err(anyhow::anyhow!("access denied")), || {
                Ok(AutostartStatus {
                    enabled: true,
                    path_matches: true,
                })
            })
            .is_err()
        );
        assert!(
            finish_disable(Err(anyhow::anyhow!("access denied")), || {
                Err(anyhow::anyhow!("status unavailable"))
            })
            .is_err()
        );
    }

    #[test]
    fn scheduler_query_failure_cannot_be_cleared_by_an_absent_registry_entry() {
        let error = finish_disable(Err(anyhow::anyhow!("delete denied")), || {
            resolve_status(Err(anyhow::anyhow!("scheduler unavailable")), || {
                panic!("An unknown task status must not fall back to the registry")
            })
        })
        .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("delete denied"));
        assert!(diagnostic.contains("scheduler unavailable"));
    }

    #[test]
    fn registry_status_is_only_used_after_a_disabled_task_is_verified() {
        let enabled = AutostartStatus {
            enabled: true,
            path_matches: true,
        };
        let status = resolve_status(Ok(enabled), || {
            panic!("An enabled task is already authoritative")
        })
        .unwrap();
        assert!(status.enabled && status.path_matches);
        let status = resolve_status(
            Ok(AutostartStatus {
                enabled: false,
                path_matches: false,
            }),
            || Ok(enabled),
        )
        .unwrap();
        assert!(status.enabled && status.path_matches);
    }

    #[test]
    fn login_arguments_always_include_the_effective_data_directory() {
        let data_dir = Path::new(r"C:\Orbment data\用户\");
        assert_eq!(
            data_dir_arguments(data_dir).unwrap(),
            [
                OsString::from("--data-dir"),
                data_dir.as_os_str().to_owned()
            ]
        );
    }

    #[test]
    fn login_arguments_resolve_relative_directories_before_registration() {
        let data_dir = Path::new("portable data");
        let arguments = data_dir_arguments(data_dir).unwrap();
        assert_eq!(arguments[0], "--data-dir");
        assert_eq!(
            Path::new(&arguments[1]),
            std::path::absolute(data_dir).unwrap()
        );
        assert!(Path::new(&arguments[1]).is_absolute());
    }

    #[test]
    fn task_xml_is_utf16_little_endian_with_bom() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task>用户𐐀</Task>";
        let temporary = TemporaryXml::create(xml).unwrap();
        let bytes = fs::read(&temporary.0).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), xml);
        let path = temporary.0.clone();
        drop(temporary);
        assert!(!path.exists());
    }
}
