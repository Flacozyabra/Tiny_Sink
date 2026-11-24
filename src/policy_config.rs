//! Safe COM wrapper for Windows `IPolicyConfig` to switch the default audio playback endpoint.

use crate::config::to_wide_null;
use std::ffi::c_void;
use std::ptr::null_mut;
use windows::core::{GUID, HRESULT, IUnknown, Interface, Result};
use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia, ERole};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

const CLSID_POLICY_CONFIG_VISTA: GUID = GUID::from_u128(0x294935ce_f637_4e7c_a41b_ab255460b862);
const IID_IPOLICY_CONFIG_VISTA: GUID = GUID::from_u128(0x568b9108_44bf_40b4_9006_86afe5b5a620);

type SetDefaultEndpointFn = unsafe extern "system" fn(
    this: *mut c_void,
    device_id: *const u16,
    role: ERole,
) -> HRESULT;

/// RAII wrapper for a raw COM interface pointer obtained via `QueryInterface`.
struct RawComPtr(*mut c_void);

impl Drop for RawComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let vtable = *(self.0 as *const *const *const c_void);
                let release_ptr = *vtable.add(2);
                let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                    std::mem::transmute(release_ptr);
                release(self.0);
            }
        }
    }
}

fn acquire_policy_config(clsid: &GUID, iid: &GUID) -> Result<RawComPtr> {
    unsafe {
        let unk: IUnknown = CoCreateInstance(clsid, None, CLSCTX_ALL)?;
        let mut raw: *mut c_void = null_mut();
        (unk.vtable().QueryInterface)(unk.as_raw(), iid, &mut raw).ok()?;
        if raw.is_null() {
            return Err(windows::core::Error::from_hresult(HRESULT(-2147467262)));
        }
        Ok(RawComPtr(raw))
    }
}

/// Sets the specified Windows audio endpoint ID as the default playback device for all roles.
pub fn set_default_audio_endpoint(device_id: &str) -> Result<()> {
    let policy_ptr = acquire_policy_config(&CLSID_POLICY_CONFIG, &IID_IPOLICY_CONFIG)
        .or_else(|_| acquire_policy_config(&CLSID_POLICY_CONFIG_VISTA, &IID_IPOLICY_CONFIG_VISTA))?;

    let wide_id = to_wide_null(device_id);
    unsafe {
        // IUnknown occupies vtable slots 0..=2; SetDefaultEndpoint sits at slot 13.
        let vtable = *(policy_ptr.0 as *const *const *const c_void);
        let fn_ptr = *vtable.add(13);
        let set_default_endpoint: SetDefaultEndpointFn = std::mem::transmute(fn_ptr);

        for role in [eConsole, eMultimedia, eCommunications] {
            set_default_endpoint(policy_ptr.0, wide_id.as_ptr(), role).ok()?;
        }
    }
    Ok(())
}

