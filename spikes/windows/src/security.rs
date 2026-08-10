use std::{fs, io, os::windows::ffi::OsStrExt, path::Path};

use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::U16CString;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WIN32_ERROR},
        Security::{
            ACCESS_ALLOWED_ACE,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                ConvertStringSidToSidW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
                SetNamedSecurityInfoW,
            },
            CONTAINER_INHERIT_ACE,
            Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
            GetSecurityDescriptorDacl, GetTokenInformation, OBJECT_INHERIT_ACE,
            OBJECT_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, PWSTR},
};

use crate::support::{SpikeResult, fail, require};

struct LocalAllocation(*mut core::ffi::c_void);

impl LocalAllocation {
    fn as_sid(&self) -> PSID {
        PSID(self.0)
    }
}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}

fn win32_result(code: WIN32_ERROR, operation: &str) -> SpikeResult {
    if code.0 == 0 {
        Ok(())
    } else {
        Err(format!(
            "{operation}: {}",
            io::Error::from_raw_os_error(code.0 as i32)
        )
        .into())
    }
}

pub fn current_user_sid_string() -> SpikeResult<String> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }

    let result = (|| {
        let mut byte_len = 0_u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut byte_len) };
        require(
            byte_len > 0,
            "GetTokenInformation did not report a TOKEN_USER size",
        )?;

        let word_len = (byte_len as usize).div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; word_len];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                byte_len,
                &mut byte_len,
            )?;
        }
        let token_user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        let mut string_sid = PWSTR::null();
        unsafe {
            ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid)?;
        }
        let allocation = LocalAllocation(string_sid.0.cast());
        let sid = unsafe { PWSTR(allocation.0.cast()).to_string()? };
        Ok(sid)
    })();

    unsafe {
        CloseHandle(token)?;
    }
    result
}

fn private_sddl(is_directory: bool) -> SpikeResult<String> {
    let sid = current_user_sid_string()?;
    let inheritance = if is_directory { "OICI" } else { "" };
    Ok(format!(
        "D:P(A;{inheritance};FA;;;{sid})(A;{inheritance};FA;;;SY)"
    ))
}

pub fn private_pipe_security_descriptor() -> SpikeResult<SecurityDescriptor> {
    let sddl = U16CString::from_str(private_sddl(false)?)?;
    Ok(SecurityDescriptor::deserialize(sddl.as_ucstr())?)
}

pub fn secure_path(path: &Path, is_directory: bool) -> SpikeResult {
    let sddl = U16CString::from_str(private_sddl(is_directory)?)?;
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    let allocation = LocalAllocation(descriptor.0);

    let mut present = false.into();
    let mut defaulted = false.into();
    let mut dacl = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)?;
    }
    require(
        present.as_bool() && !dacl.is_null(),
        "private SDDL produced no DACL",
    )?;

    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let code = unsafe {
        SetNamedSecurityInfoW(
            PWSTR(path_wide.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    drop(allocation);
    win32_result(code, "SetNamedSecurityInfoW")
}

fn sid_from_string(value: &str) -> SpikeResult<LocalAllocation> {
    let value = U16CString::from_str(value)?;
    let mut sid = PSID::default();
    unsafe {
        ConvertStringSidToSidW(PCWSTR(value.as_ptr()), &mut sid)?;
    }
    require(!sid.0.is_null(), "SID conversion returned null")?;
    Ok(LocalAllocation(sid.0))
}

fn sids_equal(left: PSID, right: PSID) -> bool {
    unsafe { EqualSid(left, right).is_ok() }
}

pub fn verify_private_path(path: &Path) -> SpikeResult {
    let expected_sid = current_user_sid_string()?;
    let current_user = sid_from_string(&expected_sid)?;
    let local_system = sid_from_string("S-1-5-18")?;
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let code = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut descriptor,
        )
    };
    win32_result(code, "GetNamedSecurityInfoW")?;
    let allocation = LocalAllocation(descriptor.0);

    let mut control = 0_u16;
    let mut revision = 0_u32;
    unsafe {
        GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)?;
    }
    require(
        control & SE_DACL_PROTECTED.0 != 0,
        format!("{} has an inheritable, unprotected DACL", path.display()),
    )?;

    let sddl = descriptor_sddl(descriptor)?;
    let mut present = false.into();
    let mut defaulted = false.into();
    let mut dacl = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)?;
    }
    require(
        present.as_bool() && !dacl.is_null(),
        format!("{} has no DACL: {sddl}", path.display()),
    )?;

    let ace_count = unsafe { (*dacl).AceCount as u32 };
    require(
        ace_count == 2,
        format!(
            "{} ACL has {ace_count} entries instead of 2: {sddl}",
            path.display()
        ),
    )?;

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    let expected_flags = if path.is_dir() {
        (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE).0 as u8
    } else {
        0
    };
    let mut current_user_entries = 0;
    let mut local_system_entries = 0;
    for index in 0..ace_count {
        let mut raw_ace = std::ptr::null_mut();
        unsafe {
            GetAce(dacl, index, &mut raw_ace)?;
        }
        require(
            !raw_ace.is_null(),
            format!("{} ACL entry {index} is null: {sddl}", path.display()),
        )?;
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        require(
            ace.Header.AceType == ACCESS_ALLOWED_ACE_TYPE,
            format!(
                "{} ACL entry {index} is not an allow entry: {sddl}",
                path.display()
            ),
        )?;
        require(
            ace.Header.AceFlags == expected_flags,
            format!(
                "{} ACL entry {index} has unexpected inheritance flags: {sddl}",
                path.display()
            ),
        )?;
        require(
            ace.Mask == FILE_ALL_ACCESS.0,
            format!(
                "{} ACL entry {index} does not grant full access: {sddl}",
                path.display()
            ),
        )?;

        let ace_sid = PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast());
        if sids_equal(ace_sid, current_user.as_sid()) {
            current_user_entries += 1;
        } else if sids_equal(ace_sid, local_system.as_sid()) {
            local_system_entries += 1;
        } else {
            return fail(format!(
                "{} ACL entry {index} grants an unexpected principal: {sddl}",
                path.display()
            ));
        }
    }

    require(
        current_user_entries == 1,
        format!(
            "{} ACL does not grant exactly one current-user entry: {sddl}",
            path.display()
        ),
    )?;
    require(
        local_system_entries == 1,
        format!(
            "{} ACL does not grant exactly one LocalSystem entry: {sddl}",
            path.display()
        ),
    )?;
    drop(allocation);
    Ok(())
}

fn descriptor_sddl(descriptor: PSECURITY_DESCRIPTOR) -> SpikeResult<String> {
    use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;

    let mut serialized = PWSTR::null();
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OBJECT_SECURITY_INFORMATION(DACL_SECURITY_INFORMATION.0),
            &mut serialized,
            None,
        )?;
    }
    if serialized.is_null() {
        return fail("security descriptor serialization returned null");
    }
    let allocation = LocalAllocation(serialized.0.cast());
    Ok(unsafe { PWSTR(allocation.0.cast()).to_string()? })
}

pub fn random_token_hex() -> SpikeResult<String> {
    let mut bytes = [0_u8; 32];
    let status = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    require(
        status.0 >= 0,
        format!("BCryptGenRandom failed with {status:?}"),
    )?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn run_spike() -> SpikeResult {
    use crate::support::TempDir;

    println!("[ACL] protected current-user/LocalSystem runtime ACLs");
    let temp = TempDir::new("prism-windows-acl")?;
    secure_path(temp.path(), true)?;
    verify_private_path(temp.path())?;

    let private_file = temp.path().join("worker.endpoint");
    fs::write(&private_file, b"private runtime state\n")?;
    secure_path(&private_file, false)?;
    verify_private_path(&private_file)?;
    println!("[ACL] PASS");
    Ok(())
}
