use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Media::Audio::{EDataFlow, ERole, eCapture, eCommunications, eConsole, eMultimedia},
        System::{SystemInformation::OSVERSIONINFOW, WinRT::RoGetActivationFactory},
    },
    core::{HRESULT, HSTRING, IInspectable_Vtbl, Interface},
};

windows_core::imp::define_interface!(
    IAudioPolicyConfigFactory,
    AudioPolicyConfigVtable,
    0xab3d4648_e242_459f_b02f_541c70306324
);
windows_core::imp::interface_hierarchy!(
    IAudioPolicyConfigFactory,
    windows_core::IUnknown,
    windows_core::IInspectable
);
windows_core::imp::define_interface!(
    IAudioPolicyConfigFactoryDownlevel,
    AudioPolicyConfigVtable,
    0x2a59116d_6c4f_45e0_a74f_707e3fef9258
);
windows_core::imp::interface_hierarchy!(
    IAudioPolicyConfigFactoryDownlevel,
    windows_core::IUnknown,
    windows_core::IInspectable
);

#[repr(C)]
pub struct AudioPolicyConfigVtable {
    base: IInspectable_Vtbl,
    // Both Windows interfaces have nineteen methods before the persisted route setter.
    preceding_methods: [usize; 19],
    set_endpoint:
        unsafe extern "system" fn(*mut c_void, u32, EDataFlow, ERole, *mut c_void) -> HRESULT,
    get_endpoint: usize,
    clear_endpoints: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OSVERSIONINFOW) -> i32;
}

pub(crate) enum AppRouting {
    Modern(IAudioPolicyConfigFactory),
    Downlevel(IAudioPolicyConfigFactoryDownlevel),
}

impl AppRouting {
    pub(crate) fn new() -> Result<Self> {
        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        let status = unsafe { RtlGetVersion(&mut version) };
        if status < 0 {
            anyhow::bail!("RtlGetVersion: NTSTATUS {status:#x}");
        }
        let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
        // The Windows AudioPolicyConfig Version21H2 boundary is build 21390, not build 19044.
        if version.dwBuildNumber >= 21390 {
            Ok(Self::Modern(
                unsafe { RoGetActivationFactory(&class) }
                    .context("RoGetActivationFactory(IAudioPolicyConfigFactory, 21390+)")?,
            ))
        } else {
            Ok(Self::Downlevel(
                unsafe { RoGetActivationFactory(&class) }
                    .context("RoGetActivationFactory(IAudioPolicyConfigFactory, downlevel)")?,
            ))
        }
    }

    pub(crate) fn route(
        &self,
        pid: u32,
        flow: EDataFlow,
        device_id: &str,
        communications: bool,
    ) -> Result<()> {
        let device = HSTRING::from(endpoint_interface_id(device_id, flow));
        let (raw, vtable) = match self {
            Self::Modern(interface) => (interface.as_raw(), interface.vtable()),
            Self::Downlevel(interface) => (interface.as_raw(), interface.vtable()),
        };
        let roles = [eConsole, eMultimedia, eCommunications];
        for role in &roles[..if communications { 3 } else { 2 }] {
            unsafe {
                (vtable.set_endpoint)(raw, pid, flow, *role, std::mem::transmute_copy(&device)).ok()
            }
            .with_context(|| {
                format!(
                    "SetPersistedDefaultAudioEndpoint(pid={pid}, role={})",
                    role.0
                )
            })?;
        }
        Ok(())
    }
}

fn endpoint_interface_id(id: &str, flow: EDataFlow) -> String {
    let suffix = if flow == eCapture {
        "#{2eef81be-33fa-4800-9670-1cd474972c3f}"
    } else {
        "#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
    };
    format!(r"\\?\SWD#MMDEVAPI#{id}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::eRender;

    #[test]
    fn persisted_routes_use_device_interface_paths_for_each_flow() {
        assert_eq!(
            endpoint_interface_id("endpoint", eRender),
            r"\\?\SWD#MMDEVAPI#endpoint#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
        assert_eq!(
            endpoint_interface_id("endpoint", eCapture),
            r"\\?\SWD#MMDEVAPI#endpoint#{2eef81be-33fa-4800-9670-1cd474972c3f}"
        );
    }
}
