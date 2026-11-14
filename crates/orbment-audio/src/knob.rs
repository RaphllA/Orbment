use anyhow::Result;
use orbment_core::config::AudioConfig;
use orbment_ui::HookThread;

/// `acquired` belongs to the app and tracks only the audio module's hook reference.
pub fn set_enabled(
    hooks: &mut HookThread,
    acquired: &mut bool,
    config: &AudioConfig,
) -> Result<()> {
    let enabled = config.enabled && config.knob_remap.intercept_volume_mute;
    if enabled && !*acquired {
        hooks.acquire()?;
        *acquired = true;
    } else if !enabled && *acquired {
        hooks.release();
        *acquired = false;
    }
    Ok(())
}

/// Resolve physical-mute hook events on the UI thread, outside the hook callback.
pub fn action(config: &AudioConfig) -> Option<&str> {
    (config.enabled && config.knob_remap.intercept_volume_mute)
        .then_some(config.knob_remap.action.as_str())
}
