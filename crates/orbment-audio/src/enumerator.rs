use std::{collections::HashSet, sync::mpsc::Sender};

use anyhow::{Context, Result};
use orbment_core::config::AudioDeviceSelection;
use windows::{
    Win32::{
        Devices::FunctionDiscovery::{
            PKEY_Device_FriendlyName, PKEY_Device_InstanceId, PKEY_DeviceClass_IconPath,
        },
        Foundation::{ERROR_NOT_FOUND, PROPERTYKEY},
        Media::Audio::{
            DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATEMASK_ALL, EDataFlow, ERole,
            Endpoints::IAudioEndpointVolume, IDeviceTopology, IMMDevice, IMMDeviceEnumerator,
            IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole,
        },
        System::Com::{
            CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
        },
    },
    core::{GUID, HRESULT, PCWSTR, PWSTR, implement},
};

use crate::{AudioDeviceMetadata, Command, cycler::Endpoints};

#[implement(IMMNotificationClient)]
struct NotificationClient {
    sender: Sender<Command>,
}

impl NotificationClient {
    fn changed(&self) {
        // COM may invoke this on any RPC thread; only the worker touches endpoints.
        let _ = self.sender.send(Command::DevicesChanged);
    }
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for NotificationClient_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        self.changed();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _: EDataFlow,
        role: ERole,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        if role == eConsole {
            self.changed();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _: &PCWSTR, key: &PROPERTYKEY) -> windows::core::Result<()> {
        if *key == PKEY_Device_FriendlyName || *key == PKEY_DeviceClass_IconPath {
            self.changed();
        }
        Ok(())
    }
}

pub(crate) struct Enumerator {
    interface: IMMDeviceEnumerator,
    notification: IMMNotificationClient,
}

impl Enumerator {
    pub(crate) fn new(sender: Sender<Command>) -> Result<Self> {
        let interface: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("CoCreateInstance(MMDeviceEnumerator)")?;
        let notification: IMMNotificationClient = NotificationClient { sender }.into();
        unsafe { interface.RegisterEndpointNotificationCallback(&notification) }
            .context("RegisterEndpointNotificationCallback")?;
        Ok(Self {
            interface,
            notification,
        })
    }

    pub(crate) fn endpoints(&self, flow: EDataFlow) -> Result<Endpoints> {
        unsafe {
            let collection = self
                .interface
                .EnumAudioEndpoints(flow, DEVICE_STATE(DEVICE_STATEMASK_ALL))?;
            let count = collection.GetCount()?;
            let mut connected = Vec::new();
            let mut offline_ids = HashSet::new();
            for index in 0..count {
                let device = collection.Item(index)?;
                let id = device_id(&device)?;
                if device.GetState()? == DEVICE_STATE_ACTIVE {
                    let name = property_string(&device, &PKEY_Device_FriendlyName)?;
                    connected.push(AudioDeviceSelection { id, name });
                } else {
                    offline_ids.insert(id);
                }
            }
            // Enumeration order can change after hotplug. IDs provide a stable All cycle.
            connected.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(Endpoints {
                connected,
                offline_ids,
            })
        }
    }

    pub(crate) fn default_id(&self, flow: EDataFlow) -> Result<Option<String>> {
        match unsafe { self.interface.GetDefaultAudioEndpoint(flow, eConsole) } {
            Ok(device) => Ok(Some(device_id(&device)?)),
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn device(&self, id: &str) -> Result<IMMDevice> {
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        Ok(unsafe { self.interface.GetDevice(PCWSTR(wide.as_ptr()))? })
    }

    pub(crate) fn volume(&self, id: &str) -> Result<IAudioEndpointVolume> {
        Ok(unsafe { self.device(id)?.Activate(CLSCTX_ALL, None)? })
    }

    pub(crate) fn metadata(&self, devices: &[AudioDeviceSelection]) -> Vec<AudioDeviceMetadata> {
        devices
            .iter()
            .map(|selection| {
                let device = self.device(&selection.id);
                let icon_path = device.as_ref().ok().and_then(|device| {
                    property_string(
                        device,
                        &PROPERTYKEY {
                            fmtid: GUID::from_u128(0x259abffc_50a7_47ce_af08_68c9a7d73366),
                            pid: 12,
                        },
                    )
                    .ok()
                });
                let usb = device
                    .as_ref()
                    .ok()
                    .and_then(|device| self.usb_transport(device).ok());
                AudioDeviceMetadata {
                    id: selection.id.clone(),
                    icon_path,
                    usb,
                }
            })
            .collect()
    }

    fn usb_transport(&self, device: &IMMDevice) -> Result<bool> {
        // Endpoint IDs describe software nodes; inspect the connected adapter's PnP identity.
        let topology: IDeviceTopology = unsafe { device.Activate(CLSCTX_ALL, None)? };
        for index in 0..unsafe { topology.GetConnectorCount()? } {
            let connector = unsafe { topology.GetConnector(index)? };
            let adapter_id = take_string(unsafe { connector.GetDeviceIdConnectedTo()? })?;
            let adapter = self.device(&adapter_id)?;
            let instance = property_string(&adapter, &PKEY_Device_InstanceId)?;
            if instance.to_ascii_uppercase().starts_with("USB\\") {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Drop for Enumerator {
    fn drop(&mut self) {
        if let Err(error) = unsafe {
            self.interface
                .UnregisterEndpointNotificationCallback(&self.notification)
        } {
            tracing::warn!(%error, "UnregisterEndpointNotificationCallback failed");
        }
    }
}

fn device_id(device: &IMMDevice) -> Result<String> {
    take_string(unsafe { device.GetId()? })
}

fn take_string(text: PWSTR) -> Result<String> {
    let result = unsafe { text.to_string() };
    unsafe { CoTaskMemFree(Some(text.0.cast())) };
    Ok(result?)
}

fn property_string(device: &IMMDevice, key: &PROPERTYKEY) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(key)?;
        let allocated = PropVariantToStringAlloc(&value);
        let cleared = PropVariantClear(&mut value);
        let text = allocated.map_err(anyhow::Error::from).and_then(take_string);
        cleared?;
        text
    }
}
