use crate::i18n::tr;
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{Foundation::ERROR_NOT_FOUND, Security::Credentials::*},
    core::{PCWSTR, PWSTR},
};

fn target_name(target: &str) -> Result<Vec<u16>> {
    ensure!(
        !target.trim().is_empty()
            && !target.contains('\0')
            && target.encode_utf16().count() <= CRED_MAX_GENERIC_TARGET_NAME_LENGTH as usize,
        "{}",
        tr("credentials.invalid_target")
    );
    Ok(target.encode_utf16().chain(Some(0)).collect())
}

struct Credential(*mut CREDENTIALW);
impl Drop for Credential {
    fn drop(&mut self) {
        unsafe {
            // Erase the API-owned secret before releasing its allocation.
            let credential = &mut *self.0;
            for index in 0..credential.CredentialBlobSize as usize {
                std::ptr::write_volatile(credential.CredentialBlob.add(index), 0);
            }
            CredFree(self.0.cast());
        }
    }
}

/// Generic credentials contain UTF-8 bytes, scoped to the current Windows user.
/// Absence is distinct from access, storage, or decoding failures.
pub fn read_api_key(target: &str) -> Result<Option<String>> {
    let target = target_name(target)?;
    let mut pointer = std::ptr::null_mut();
    match unsafe {
        CredReadW(
            PCWSTR(target.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut pointer,
        )
    } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(None),
        Err(error) => return Err(error).context(tr("credentials.read_failed")),
    }
    let credential = Credential(pointer);
    let value = unsafe { &*credential.0 };
    let bytes = if value.CredentialBlobSize == 0 {
        &[][..]
    } else {
        unsafe {
            std::slice::from_raw_parts(value.CredentialBlob, value.CredentialBlobSize as usize)
        }
    };
    Ok(Some(
        std::str::from_utf8(bytes)
            .context(tr("credentials.invalid_encoding"))?
            .to_owned(),
    ))
}

/// Only the official OpenAI default target inherits the pre-channel credential.
pub fn read_quick_ask_api_key(config: &crate::config::QuickAskConfig) -> Result<Option<String>> {
    let key = read_api_key(&config.credential_target)?;
    if key.is_none()
        && config.provider == crate::config::QuickAskProvider::OpenAI
        && config.credential_target == config.provider.default_credential_target()
    {
        return read_api_key("Orbment/QuickAsk/default");
    }
    Ok(key)
}

pub fn write_api_key(target: &str, key: &str) -> Result<()> {
    let mut target = target_name(target)?;
    ensure!(
        !key.trim().is_empty()
            && key.len() <= CRED_MAX_CREDENTIAL_BLOB_SIZE as usize
            && !key.contains(['\r', '\n', '\0']),
        "{}",
        tr("credentials.invalid_key")
    );
    let mut username: Vec<u16> = "Orbment".encode_utf16().chain(Some(0)).collect();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        UserName: PWSTR(username.as_mut_ptr()),
        CredentialBlobSize: key.len() as u32,
        CredentialBlob: key.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&credential, 0) }.context(tr("credentials.write_failed"))
}

/// Returns false only when the credential did not exist.
pub fn delete_api_key(target: &str) -> Result<bool> {
    let target = target_name(target)?;
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(true),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(false),
        Err(error) => Err(error).context(tr("credentials.delete_failed")),
    }
}
