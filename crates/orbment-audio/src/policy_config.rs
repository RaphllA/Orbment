use std::ffi::c_void;

use anyhow::{Context, Result};
use orbment_core::i18n::tr;
use windows::{
    Win32::{
        Media::Audio::{ERole, eCommunications, eConsole, eMultimedia},
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    },
    core::{GUID, HRESULT, IUnknown_Vtbl, Interface, PCWSTR},
};

windows_core::imp::define_interface!(
    IPolicyConfig,
    PolicyConfigVtable,
    0xf8679f50_850a_41cf_9c72_430f290290c8
);
windows_core::imp::interface_hierarchy!(IPolicyConfig, windows_core::IUnknown);

#[repr(C)]
pub struct PolicyConfigVtable {
    base: IUnknown_Vtbl,
    // Ten policy methods precede SetDefaultEndpoint after IUnknown's three slots.
    preceding_methods: [usize; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
    set_endpoint_visibility: usize,
}

const POLICY_CLASS: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

pub(crate) struct PolicyConfig(IPolicyConfig);

impl PolicyConfig {
    pub(crate) fn new() -> Result<Self> {
        let interface = unsafe { CoCreateInstance(&POLICY_CLASS, None, CLSCTX_ALL) }
            .context("CoCreateInstance(IPolicyConfig)")?;
        Ok(Self(interface))
    }

    pub(crate) fn set_default(&self, id: &str, communications: bool) -> Result<()> {
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        let roles = [eConsole, eMultimedia, eCommunications];
        let count = if communications { 3 } else { 2 };
        for role in &roles[..count] {
            // Policy updates are sequential, so an error can leave earlier roles changed.
            unsafe {
                (self.0.vtable().set_default_endpoint)(
                    self.0.as_raw(),
                    PCWSTR(wide.as_ptr()),
                    *role,
                )
                .ok()
            }
            .with_context(|| format!("{} (role={})", tr("audio.switch_failed"), role.0))?;
        }
        Ok(())
    }
}
