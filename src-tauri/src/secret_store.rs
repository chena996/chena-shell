//! Windows-backed secret storage.
//!
//! `data.0` keeps only stable credential references. Passwords and private-key
//! passphrases live in Windows Credential Manager; the random AES master key is
//! protected by DPAPI for the current user.

use std::path::PathBuf;

use rand::RngCore;

const REFERENCE_PREFIX: &str = "cred:v1:";
const MASTER_FILE_MAGIC: &[u8] = b"CSMK1\0";
const MAX_SECRET_BYTES: usize = 64 * 1024;

pub fn credential_target(session_id: &str, kind: &str) -> String {
    format!("chena-shell/session/{session_id}/{kind}")
}

#[cfg(test)]
pub fn reference_for(session_id: &str, kind: &str) -> String {
    format!("{REFERENCE_PREFIX}{}", credential_target(session_id, kind))
}

pub fn is_reference(value: &str) -> bool {
    value.starts_with(REFERENCE_PREFIX)
}

pub fn store_session_secret(session_id: &str, kind: &str, secret: &str) -> Result<String, String> {
    if secret.is_empty() {
        return Err("不能保存空秘密".into());
    }
    if secret.len() > MAX_SECRET_BYTES {
        return Err("秘密内容超过 64 KiB 限制".into());
    }
    // 每次替换使用新目标：只有配置原子保存成功后才删除旧目标，
    // 因此落盘失败可以恢复旧配置而不会丢失旧秘密。
    let target = format!(
        "{}/{}",
        credential_target(session_id, kind),
        uuid::Uuid::new_v4().simple()
    );
    platform::write(&target, secret.as_bytes())?;
    let verified = match platform::read(&target) {
        Ok(verified) => verified,
        Err(error) => {
            let _ = platform::remove(&target);
            return Err(error);
        }
    };
    if verified != secret.as_bytes() {
        let _ = platform::remove(&target);
        return Err("秘密写入后回读校验失败，未替换配置".into());
    }
    Ok(format!("{REFERENCE_PREFIX}{target}"))
}

pub fn resolve_secret(value: &str) -> Result<String, String> {
    let Some(target) = value.strip_prefix(REFERENCE_PREFIX) else {
        // 仅用于尚未完成一次性迁移的旧内存配置；新写入始终使用引用。
        return Ok(value.to_string());
    };
    if target.is_empty() || target.len() > 512 {
        return Err("Credential Manager 引用无效".into());
    }
    let bytes = platform::read(target)?;
    String::from_utf8(bytes).map_err(|_| "Credential Manager 中的秘密不是有效 UTF-8".into())
}

pub fn delete_reference(value: &str) -> Result<(), String> {
    let Some(target) = value.strip_prefix(REFERENCE_PREFIX) else {
        return Ok(());
    };
    platform::remove(target)
}

pub fn load_or_create_master_key() -> Result<[u8; 32], String> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let path = master_key_path();
    if path.exists() {
        return read_master_key(&path);
    }

    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    let protected = platform::protect(&key)?;
    let mut bytes = Vec::with_capacity(MASTER_FILE_MAGIC.len() + protected.len());
    bytes.extend_from_slice(MASTER_FILE_MAGIC);
    bytes.extend_from_slice(&protected);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建密钥目录失败: {error}"))?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, &bytes).map_err(|error| format!("写入受保护主密钥失败: {error}"))?;
    if path.exists() {
        let _ = std::fs::remove_file(&temp);
        return read_master_key(&path);
    }
    std::fs::rename(&temp, &path).map_err(|error| format!("保存受保护主密钥失败: {error}"))?;
    // 保存后立即解保护验证，避免把不可用的密钥用于新配置。
    read_master_key(&path)?;
    Ok(key)
}

fn master_key_path() -> PathBuf {
    crate::state::app_dir().join("master.key")
}

fn read_master_key(path: &std::path::Path) -> Result<[u8; 32], String> {
    let bytes = std::fs::read(path).map_err(|error| format!("读取受保护主密钥失败: {error}"))?;
    let protected = bytes
        .strip_prefix(MASTER_FILE_MAGIC)
        .ok_or("主密钥文件格式无效")?;
    let key = platform::unprotect(protected)?;
    key.try_into().map_err(|_| "主密钥长度无效".into())
}

#[cfg(windows)]
mod platform {
    use super::MAX_SECRET_BYTES;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;
    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    #[repr(C)]
    struct DataBlob {
        size: u32,
        data: *mut u8,
    }

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct CredentialW {
        flags: u32,
        kind: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: FileTime,
        blob_size: u32,
        blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            input: *mut DataBlob,
            description: *const u16,
            entropy: *mut DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *mut DataBlob,
            description: *mut *mut u16,
            entropy: *mut DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
        fn CredReadW(
            target: *const u16,
            kind: u32,
            flags: u32,
            credential: *mut *mut CredentialW,
        ) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *const c_void);
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    pub fn protect(input: &[u8]) -> Result<Vec<u8>, String> {
        let mut input_blob = DataBlob {
            size: input.len() as u32,
            data: input.as_ptr() as *mut u8,
        };
        let description = wide("Chena-Shell master key");
        let mut output = DataBlob {
            size: 0,
            data: std::ptr::null_mut(),
        };
        // SAFETY: pointers reference live buffers for the duration of the FFI call;
        // Windows allocates `output`, which is copied and released immediately.
        let ok = unsafe {
            CryptProtectData(
                &mut input_blob,
                description.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI 保护主密钥失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: successful DPAPI returned a valid buffer of `size` bytes.
        let bytes =
            unsafe { std::slice::from_raw_parts(output.data, output.size as usize) }.to_vec();
        unsafe { LocalFree(output.data.cast()) };
        Ok(bytes)
    }

    pub fn unprotect(input: &[u8]) -> Result<Vec<u8>, String> {
        let mut input_blob = DataBlob {
            size: input.len() as u32,
            data: input.as_ptr() as *mut u8,
        };
        let mut output = DataBlob {
            size: 0,
            data: std::ptr::null_mut(),
        };
        // SAFETY: same contract as `protect`; no UI is allowed during background load.
        let ok = unsafe {
            CryptUnprotectData(
                &mut input_blob,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI 解保护主密钥失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.data, output.size as usize) }.to_vec();
        unsafe { LocalFree(output.data.cast()) };
        Ok(bytes)
    }

    pub fn write(target: &str, secret: &[u8]) -> Result<(), String> {
        if secret.len() > MAX_SECRET_BYTES {
            return Err("秘密内容超过 Credential Manager 限制".into());
        }
        let target_wide = wide(target);
        let user_wide = wide("Chena-Shell");
        let credential = CredentialW {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: target_wide.as_ptr() as *mut u16,
            comment: std::ptr::null_mut(),
            last_written: FileTime { low: 0, high: 0 },
            blob_size: secret.len() as u32,
            blob: secret.as_ptr() as *mut u8,
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: std::ptr::null_mut(),
            target_alias: std::ptr::null_mut(),
            user_name: user_wide.as_ptr() as *mut u16,
        };
        // SAFETY: all pointers in CREDENTIALW remain valid during CredWriteW.
        let ok = unsafe { CredWriteW(&credential, 0) };
        if ok == 0 {
            return Err(format!(
                "写入 Windows Credential Manager 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub fn read(target: &str) -> Result<Vec<u8>, String> {
        let target_wide = wide(target);
        let mut credential: *mut CredentialW = std::ptr::null_mut();
        // SAFETY: CredReadW allocates a credential which is copied then freed with CredFree.
        let ok = unsafe { CredReadW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 || credential.is_null() {
            return Err(format!(
                "读取 Windows Credential Manager 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let result = unsafe {
            let item = &*credential;
            if item.blob_size as usize > MAX_SECRET_BYTES
                || (item.blob_size > 0 && item.blob.is_null())
            {
                Err("Credential Manager 返回了无效的秘密数据".into())
            } else {
                Ok(std::slice::from_raw_parts(item.blob, item.blob_size as usize).to_vec())
            }
        };
        unsafe { CredFree(credential.cast()) };
        result
    }

    pub fn remove(target: &str) -> Result<(), String> {
        let target_wide = wide(target);
        let ok = unsafe { CredDeleteW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            return Err(format!(
                "删除 Windows Credential Manager 秘密失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn protect(_input: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台尚未配置系统密钥链，拒绝以明文保存主密钥".into())
    }

    pub fn unprotect(_input: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台尚未配置系统密钥链".into())
    }

    pub fn write(_target: &str, _secret: &[u8]) -> Result<(), String> {
        Err("当前平台尚未配置系统秘密存储，拒绝明文写入".into())
    }

    pub fn read(_target: &str) -> Result<Vec<u8>, String> {
        Err("当前平台尚未配置系统秘密存储".into())
    }

    pub fn remove(_target: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_contain_no_secret_and_legacy_values_remain_compatible() {
        let reference = store_reference_only("session-1", "password");
        assert!(is_reference(&reference));
        assert!(!reference.contains("correct horse"));
        assert_eq!(resolve_secret("correct horse").unwrap(), "correct horse");
    }

    fn store_reference_only(session_id: &str, kind: &str) -> String {
        reference_for(session_id, kind)
    }

    #[test]
    fn malformed_reference_fails_closed() {
        assert!(resolve_secret("cred:v1:").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_dpapi_round_trip_uses_current_user_protection() {
        let plaintext = b"chena-shell-dpapi-test-key-material";
        let protected = platform::protect(plaintext).unwrap();
        assert_ne!(protected.as_slice(), plaintext);
        assert_eq!(platform::unprotect(&protected).unwrap(), plaintext);
    }

    #[cfg(windows)]
    #[test]
    fn windows_credential_manager_write_read_and_delete_round_trip() {
        let session_id = format!("acceptance-{}", uuid::Uuid::new_v4().simple());
        let secret = "credential-manager-round-trip-secret";
        let reference = store_session_secret(&session_id, "password", secret).unwrap();
        assert!(is_reference(&reference));
        assert!(!reference.contains(secret));
        assert_eq!(resolve_secret(&reference).unwrap(), secret);
        delete_reference(&reference).unwrap();
        assert!(resolve_secret(&reference).is_err());
    }
}
