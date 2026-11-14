use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
};

use anyhow::Result;
use windows::{
    Win32::Media::Audio::{
        AUDIO_VOLUME_NOTIFICATION_DATA,
        Endpoints::{
            IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
        },
    },
    core::implement,
};

use crate::Command;

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    sender: Sender<Command>,
    muted: AtomicBool,
}

#[allow(non_snake_case)]
impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        let Some(data) = (unsafe { data.as_ref() }) else {
            return Ok(());
        };
        let muted = data.bMuted.as_bool();
        if self.muted.swap(muted, Ordering::Relaxed) != muted {
            // Volume-only notifications do not require a default-endpoint query.
            let _ = self.sender.send(Command::MicrophoneChanged);
        }
        Ok(())
    }
}

pub(crate) struct MicrophoneObserver {
    pub(crate) id: String,
    pub(crate) endpoint: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}

impl MicrophoneObserver {
    pub(crate) fn new(
        id: String,
        endpoint: IAudioEndpointVolume,
        sender: Sender<Command>,
    ) -> Result<Self> {
        let muted = unsafe { endpoint.GetMute()? }.as_bool();
        let callback: IAudioEndpointVolumeCallback = VolumeCallback {
            sender,
            muted: AtomicBool::new(muted),
        }
        .into();
        unsafe { endpoint.RegisterControlChangeNotify(&callback)? };
        Ok(Self {
            id,
            endpoint,
            callback,
        })
    }

    pub(crate) fn muted(&self) -> Result<bool> {
        Ok(unsafe { self.endpoint.GetMute()? }.as_bool())
    }

    pub(crate) fn toggle(&self) -> Result<()> {
        unsafe { self.endpoint.SetMute(!self.muted()?, std::ptr::null())? };
        Ok(())
    }
}

impl Drop for MicrophoneObserver {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.endpoint.UnregisterControlChangeNotify(&self.callback) } {
            tracing::warn!(%error, "UnregisterControlChangeNotify failed");
        }
    }
}
