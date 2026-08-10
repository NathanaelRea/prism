use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
    OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

struct LocalAllocation(*mut core::ffi::c_void);

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this handle was returned by CreateFileW and is owned by this value.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this allocation was returned by a Windows LocalAlloc-family API.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}

fn win32_result(code: WIN32_ERROR) -> io::Result<()> {
    if code.0 == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code.0 as i32))
    }
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: token is a valid output pointer and is closed before returning.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(io::Error::other)?;
    let result = (|| {
        let mut byte_len = 0_u32;
        // The first call intentionally obtains the required size.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut byte_len) };
        if byte_len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0_usize; (byte_len as usize).div_ceil(size_of::<usize>())];
        // SAFETY: storage is aligned and at least byte_len bytes long.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                byte_len,
                &mut byte_len,
            )
        }
        .map_err(io::Error::other)?;
        let token_user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        let mut string_sid = PWSTR::null();
        // SAFETY: the token-owned SID remains valid for this call.
        unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid) }
            .map_err(io::Error::other)?;
        let allocation = LocalAllocation(string_sid.0.cast());
        // SAFETY: ConvertSidToStringSidW returned a terminated UTF-16 string.
        unsafe { PWSTR(allocation.0.cast()).to_string() }.map_err(io::Error::other)
    })();
    // SAFETY: token was opened above and is no longer used.
    unsafe { CloseHandle(token) }.map_err(io::Error::other)?;
    result
}

fn private_sddl(is_directory: bool) -> io::Result<String> {
    let inheritance = if is_directory { "OICI" } else { "" };
    Ok(format!(
        "D:P(A;{inheritance};FA;;;{})(A;{inheritance};FA;;;SY)",
        current_user_sid_string()?
    ))
}

pub(crate) fn private_pipe_security_descriptor() -> Result<SecurityDescriptor, String> {
    let sddl = widestring(private_sddl(false).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    SecurityDescriptor::deserialize(sddl.as_ucstr())
        .map_err(|error| format!("create pipe security descriptor: {error}"))
}

fn open_path_without_reparse(
    path: &Path,
    expected_directory: Option<bool>,
    write_dacl: bool,
) -> io::Result<Vec<OwnedHandle>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let ancestors = absolute
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    let ancestor_count = ancestors.len();
    let mut handles = Vec::with_capacity(ancestor_count);
    for (index, ancestor) in ancestors.into_iter().rev().enumerate() {
        let is_final = index + 1 == ancestor_count;
        let wide_path = ancestor
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let desired_access = if is_final {
            FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0 | if write_dacl { WRITE_DAC.0 } else { 0 }
        } else {
            FILE_READ_ATTRIBUTES.0
        };
        // Omitting FILE_SHARE_DELETE holds every checked ancestor stable until the final handle
        // has been opened and inspected. OPEN_REPARSE_POINT exposes a final link itself.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                desired_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(io::Error::other)?;
        let handle = OwnedHandle(handle);
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: handle is live and information is writable for the duration of this call.
        unsafe { GetFileInformationByHandle(handle.0, &mut information) }
            .map_err(io::Error::other)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "managed path traverses a Windows reparse point at {}",
                    ancestor.display()
                ),
            ));
        }
        let is_directory = information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
        if !is_final && !is_directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "managed path ancestor is not a directory: {}",
                    ancestor.display()
                ),
            ));
        }
        if is_final && expected_directory.is_some_and(|expected| expected != is_directory) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("managed path type changed while opening {}", path.display()),
            ));
        }
        handles.push(handle);
    }
    if handles.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed path is empty",
        ));
    }
    Ok(handles)
}

pub(crate) fn secure_path(path: &Path, is_directory: bool) -> io::Result<()> {
    let handles = open_path_without_reparse(path, Some(is_directory), true)?;
    let handle = handles
        .last()
        .ok_or_else(|| io::Error::other("managed path handle is missing"))?;
    secure_handle(handle.0, is_directory)
}

pub(crate) fn secure_file(file: &std::fs::File, is_directory: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    let handle = HANDLE(file.as_raw_handle());
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the borrowed file keeps the handle live during inspection.
    unsafe { GetFileInformationByHandle(handle, &mut information) }.map_err(io::Error::other)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed file handle refers to a Windows reparse point",
        ));
    }
    let is_open_directory = information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if is_open_directory != is_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed file type changed while opening",
        ));
    }
    secure_handle(handle, is_directory)
}

pub(crate) fn reject_reparse_point(path: &Path) -> io::Result<()> {
    open_path_without_reparse(path, None, false).map(|_| ())
}

fn secure_handle(handle: HANDLE, is_directory: bool) -> io::Result<()> {
    let sddl = widestring(private_sddl(is_directory)?)?;
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: sddl is terminated and descriptor is an output pointer.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(io::Error::other)?;
    let _allocation = LocalAllocation(descriptor.0);
    let mut present = false.into();
    let mut defaulted = false.into();
    let mut dacl = std::ptr::null_mut();
    // SAFETY: descriptor remains owned by allocation for the duration of this call.
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .map_err(io::Error::other)?;
    if !present.as_bool() || dacl.is_null() {
        return Err(io::Error::other("private security descriptor has no DACL"));
    }
    // SAFETY: handle is live and dacl remains owned by allocation during this call.
    let result = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    win32_result(result)?;
    verify_private_handle(handle, is_directory)
}

/// Verifies the security boundary after applying it instead of assuming the ACL API succeeded.
/// Prism-managed private paths allow exactly the current user and LocalSystem.
#[cfg(test)]
pub(crate) fn verify_private_path(path: &Path, is_directory: bool) -> io::Result<()> {
    let handles = open_path_without_reparse(path, Some(is_directory), false)?;
    let handle = handles
        .last()
        .ok_or_else(|| io::Error::other("managed path handle is missing"))?;
    verify_private_handle(handle.0, is_directory)
}

fn verify_private_handle(handle: HANDLE, is_directory: bool) -> io::Result<()> {
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: handle is live and all returned pointers are owned by descriptor.
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    };
    win32_result(code)?;
    let allocation = LocalAllocation(descriptor.0);

    let expected_owner = current_user_sid_string()?;
    let owner_string = sid_string(owner)?;
    if owner_string != expected_owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private path is owned by {owner_string}, not the current user"),
        ));
    }

    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor remains valid while allocation is alive.
    unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }
        .map_err(io::Error::other)?;
    if control & SE_DACL_PROTECTED.0 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private path DACL is not protected from inheritance",
        ));
    }

    let mut present = false.into();
    let mut defaulted = false.into();
    let mut dacl = std::ptr::null_mut();
    // SAFETY: descriptor remains valid while allocation is alive.
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .map_err(io::Error::other)?;
    if !present.as_bool() || dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private path has no DACL",
        ));
    }

    let current_user = sid_allocation(&expected_owner)?;
    let local_system = sid_allocation("S-1-5-18")?;
    let expected_flags = if is_directory {
        (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE).0 as u8
    } else {
        0
    };
    // SAFETY: dacl belongs to descriptor and is valid for this synchronous inspection.
    let ace_count = unsafe { (*dacl).AceCount as u32 };
    if ace_count != 2 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private path DACL has {ace_count} entries instead of 2"),
        ));
    }
    let mut user_entries = 0;
    let mut system_entries = 0;
    for index in 0..ace_count {
        let mut raw_ace = std::ptr::null_mut();
        // SAFETY: index is bounded by AceCount and raw_ace is an output pointer.
        unsafe { GetAce(dacl, index, &mut raw_ace) }.map_err(io::Error::other)?;
        if raw_ace.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DACL contains a null ACE",
            ));
        }
        // SAFETY: the SDDL above creates ACCESS_ALLOWED_ACE entries.
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || ace.Header.AceFlags != expected_flags
            || ace.Mask != FILE_ALL_ACCESS.0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private path DACL contains an unexpected permission entry",
            ));
        }
        let sid = PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast());
        // SAFETY: ACE SIDs and converted SIDs are valid for these calls.
        if unsafe { EqualSid(sid, current_user.as_sid()) }.is_ok() {
            user_entries += 1;
        } else if unsafe { EqualSid(sid, local_system.as_sid()) }.is_ok() {
            system_entries += 1;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private path DACL grants an unexpected principal",
            ));
        }
    }
    if user_entries != 1 || system_entries != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private path DACL does not grant exactly the current user and LocalSystem",
        ));
    }
    drop(allocation);
    Ok(())
}

struct SidAllocation(LocalAllocation);

impl SidAllocation {
    fn as_sid(&self) -> PSID {
        PSID(self.0.0)
    }
}

fn sid_allocation(value: &str) -> io::Result<SidAllocation> {
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;

    let value = widestring(value.to_string())?;
    let mut sid = PSID::default();
    // SAFETY: value is terminated and sid is an output pointer freed by LocalFree.
    unsafe { ConvertStringSidToSidW(PCWSTR(value.as_ptr()), &mut sid) }
        .map_err(io::Error::other)?;
    if sid.0.is_null() {
        return Err(io::Error::other("SID conversion returned null"));
    }
    Ok(SidAllocation(LocalAllocation(sid.0)))
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut value = PWSTR::null();
    // SAFETY: sid comes from a live security descriptor.
    unsafe { ConvertSidToStringSidW(sid, &mut value) }.map_err(io::Error::other)?;
    let allocation = LocalAllocation(value.0.cast());
    // SAFETY: ConvertSidToStringSidW returned a terminated UTF-16 string.
    unsafe { PWSTR(allocation.0.cast()).to_string() }.map_err(io::Error::other)
}

pub(crate) fn random_token_hex() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn widestring(value: String) -> io::Result<widestring::U16CString> {
    widestring::U16CString::from_str(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_private_acl_is_applied_and_verified_for_directories_and_files() {
        let root = std::env::temp_dir().join(format!(
            "prism-windows-security-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        secure_path(&root, true).unwrap();
        verify_private_path(&root, true).unwrap();
        let file = root.join("private.endpoint");
        std::fs::write(&file, b"private\n").unwrap();
        secure_path(&file, false).unwrap();
        verify_private_path(&file, false).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_reparse_ancestor_is_rejected_before_private_path_use() {
        use std::os::windows::fs::symlink_dir;

        let root = std::env::temp_dir().join(format!(
            "prism-windows-reparse-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        let target = root.join("target");
        let link = root.join("managed-link");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("private.endpoint"), b"not private\n").unwrap();
        match symlink_dir(&target, &link) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("SKIP: creating a Windows directory symlink requires developer mode");
                std::fs::remove_dir_all(root).unwrap();
                return;
            }
            Err(error) => panic!("create Windows directory symlink: {error}"),
        }

        let error = reject_reparse_point(&link.join("private.endpoint")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
