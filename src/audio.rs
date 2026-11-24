//! Windows Core Audio (MMDevice / WASAPI) enumeration, classification, and event callbacks.

use crate::config::{DeviceKind, EndpointSummary};
use windows::core::{implement, Result, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HWND, LPARAM, PROPERTYKEY, WPARAM};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, DigitalAudioDisplayDevice, EDataFlow, ERole, EndpointFormFactor,
    Headphones, Headset, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, PKEY_AudioEndpoint_FormFactor,
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PropVariantToStringAlloc, PropVariantToUInt32, PROPVARIANT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted when default device volume or mute changes via `IAudioEndpointVolumeCallback`.
pub const WM_APP_VOLUME_CHANGED: u32 = WM_APP + 1;
/// Posted when active devices, default endpoint, or device properties change.
pub const WM_APP_DEVICES_CHANGED: u32 = WM_APP + 2;

/// Snapshot of an active audio render endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub detected_kind: DeviceKind,
    pub is_display_output: bool,
    pub is_default: bool,
}

impl AudioDeviceInfo {
    pub fn to_summary(&self) -> EndpointSummary {
        EndpointSummary {
            id: self.id.clone(),
            is_display_output: self.is_display_output,
        }
    }
}

/// Snapshot of the current default audio output state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSnapshot {
    pub devices: Vec<AudioDeviceInfo>,
    pub default_device_id: Option<String>,
    pub default_device_name: String,
    pub detected_kind: DeviceKind,
    pub volume_percent: u8,
    pub muted: bool,
}

/// Pure heuristic combining Windows `EndpointFormFactor` and endpoint name keywords.
pub fn classify_device(form_factor: EndpointFormFactor, friendly_name: &str) -> (DeviceKind, bool) {
    let is_display = form_factor == DigitalAudioDisplayDevice;
    let lower = friendly_name.to_lowercase();

    const HEADPHONE_CUES: [&str; 9] = [
        "headphone",
        "headset",
        "earphone",
        "earbud",
        "airpods",
        "buds",
        "наушник",
        "гарнитур",
        "hands-free",
    ];

    if HEADPHONE_CUES.iter().any(|cue| lower.contains(cue)) {
        return (DeviceKind::Headphones, is_display);
    }

    if form_factor == Headphones || form_factor == Headset {
        return (DeviceKind::Headphones, is_display);
    }

    (DeviceKind::Speakers, is_display)
}

#[implement(IMMNotificationClient)]
struct DeviceNotificationClient {
    hwnd_raw: usize,
}

impl DeviceNotificationClient {
    fn post_devices_changed(&self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd_raw as *mut _)),
                WM_APP_DEVICES_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

impl IMMNotificationClient_Impl for DeviceNotificationClient_Impl {
    fn OnDeviceStateChanged(&self, _pwstrdeviceid: &PCWSTR, _dwnewstate: DEVICE_STATE) -> Result<()> {
        self.post_devices_changed();
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> {
        self.post_devices_changed();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> {
        self.post_devices_changed();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _pwstrdefaultdeviceid: &PCWSTR,
    ) -> Result<()> {
        if flow == eRender {
            self.post_devices_changed();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _pwstrdeviceid: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        self.post_devices_changed();
        Ok(())
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallbackClient {
    hwnd_raw: usize,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeCallbackClient_Impl {
    fn OnNotify(&self, _pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd_raw as *mut _)),
                WM_APP_VOLUME_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
        Ok(())
    }
}

/// Manages Core Audio enumeration and active COM notification subscriptions.
pub struct AudioMonitor {
    hwnd_raw: usize,
    enumerator: IMMDeviceEnumerator,
    notification_client: Option<IMMNotificationClient>,
    subscribed_device_id: Option<String>,
    endpoint_volume: Option<IAudioEndpointVolume>,
    volume_callback: Option<IAudioEndpointVolumeCallback>,
}

impl AudioMonitor {
    /// Creates a new `AudioMonitor` bound to `hwnd` for event-driven notifications.
    pub fn new(hwnd: HWND) -> Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

        let hwnd_raw = hwnd.0 as usize;
        let client: IMMNotificationClient = DeviceNotificationClient { hwnd_raw }.into();
        unsafe {
            enumerator.RegisterEndpointNotificationCallback(&client)?;
        }

        let mut monitor = Self {
            hwnd_raw,
            enumerator,
            notification_client: Some(client),
            subscribed_device_id: None,
            endpoint_volume: None,
            volume_callback: None,
        };
        monitor.sync_volume_subscription();
        Ok(monitor)
    }

    /// Reads all active playback devices, the default device, volume, and mute state.
    pub fn snapshot(&mut self) -> AudioSnapshot {
        self.sync_volume_subscription();

        let default_id = self.read_default_device_id();
        let devices = self.enumerate_active_devices(default_id.as_deref());

        let (default_name, detected_kind) = devices
            .iter()
            .find(|d| d.is_default)
            .map(|d| (d.name.clone(), d.detected_kind))
            .unwrap_or_else(|| ("Нет аудиоустройства".to_string(), DeviceKind::Speakers));

        let (volume_percent, muted) = self.read_volume_and_mute();

        AudioSnapshot {
            devices,
            default_device_id: default_id,
            default_device_name: default_name,
            detected_kind,
            volume_percent,
            muted,
        }
    }

    /// Re-attaches `IAudioEndpointVolumeCallback` if the default render endpoint changed.
    pub fn sync_volume_subscription(&mut self) {
        let default_device = unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()
        };
        let new_id = default_device.as_ref().and_then(read_device_id);

        if new_id == self.subscribed_device_id {
            return;
        }

        self.clear_volume_subscription();

        if let Some(device) = default_device {
            if let Ok(ep_vol) = unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) } {
                let cb: IAudioEndpointVolumeCallback = VolumeCallbackClient {
                    hwnd_raw: self.hwnd_raw,
                }
                .into();
                if unsafe { ep_vol.RegisterControlChangeNotify(&cb) }.is_ok() {
                    self.subscribed_device_id = new_id;
                    self.endpoint_volume = Some(ep_vol);
                    self.volume_callback = Some(cb);
                }
            }
        }
    }

    /// Reads the current master volume `[0..=100]` and mute state from the active subscription.
    pub fn read_volume_and_mute(&self) -> (u8, bool) {
        let Some(ep_vol) = self.endpoint_volume.as_ref() else {
            return (0, true);
        };
        unsafe {
            let scalar = ep_vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
            let percent = (scalar.clamp(0.0, 1.0) * 100.0).round() as u8;
            let muted = ep_vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            (percent, muted)
        }
    }

    /// Toggles mute on the current default output device.
    pub fn toggle_mute(&self) {
        if let Some(ep_vol) = self.endpoint_volume.as_ref() {
            unsafe {
                let currently_muted = ep_vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                let _ = ep_vol.SetMute(!currently_muted, std::ptr::null());
            }
        }
    }

    fn clear_volume_subscription(&mut self) {
        if let (Some(ep_vol), Some(cb)) = (self.endpoint_volume.take(), self.volume_callback.take())
        {
            unsafe {
                let _ = ep_vol.UnregisterControlChangeNotify(&cb);
            }
        }
        self.subscribed_device_id = None;
    }

    fn read_default_device_id(&self) -> Option<String> {
        unsafe {
            let device = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()?;
            read_device_id(&device)
        }
    }

    fn enumerate_active_devices(&self, default_id: Option<&str>) -> Vec<AudioDeviceInfo> {
        let mut out = Vec::new();
        let Ok(collection) =
            (unsafe { self.enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) })
        else {
            return out;
        };
        let count = unsafe { collection.GetCount().unwrap_or(0) };
        for i in 0..count {
            let Ok(device) = (unsafe { collection.Item(i) }) else {
                continue;
            };
            let Some(id) = read_device_id(&device) else {
                continue;
            };
            let (name, form_factor) = read_device_properties(&device);
            let (detected_kind, is_display_output) = classify_device(form_factor, &name);
            let is_default = default_id.is_some_and(|def| def == id);
            out.push(AudioDeviceInfo {
                id,
                name,
                detected_kind,
                is_display_output,
                is_default,
            });
        }
        out
    }
}

impl Drop for AudioMonitor {
    fn drop(&mut self) {
        self.clear_volume_subscription();
        if let Some(client) = self.notification_client.take() {
            unsafe {
                let _ = self.enumerator.UnregisterEndpointNotificationCallback(&client);
            }
        }
    }
}

fn read_device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let pwstr: PWSTR = device.GetId().ok()?;
        let s = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        s
    }
}

struct PropVariantGuard(PROPVARIANT);

impl Drop for PropVariantGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = PropVariantClear(&mut self.0);
        }
    }
}

fn read_device_properties(device: &IMMDevice) -> (String, EndpointFormFactor) {
    let Ok(store) = (unsafe { device.OpenPropertyStore(STGM_READ) }) else {
        return ("Audio Device".to_string(), EndpointFormFactor(10));
    };

    let name = unsafe {
        store
            .GetValue(&PKEY_Device_FriendlyName)
            .ok()
            .map(PropVariantGuard)
            .and_then(|pv| propvariant_to_string(&pv.0))
            .unwrap_or_else(|| "Audio Device".to_string())
    };

    let form_factor = unsafe {
        store
            .GetValue(&PKEY_AudioEndpoint_FormFactor)
            .ok()
            .map(PropVariantGuard)
            .and_then(|pv| PropVariantToUInt32(&pv.0).ok())
            .map(|v| EndpointFormFactor(v as i32))
            .unwrap_or(EndpointFormFactor(10))
    };

    (name, form_factor)
}

unsafe fn propvariant_to_string(propvar: &PROPVARIANT) -> Option<String> {
    let pwstr = PropVariantToStringAlloc(propvar).ok()?;
    let s = pwstr.to_string().ok();
    CoTaskMemFree(Some(pwstr.0 as *const _));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::{Speakers, UnknownFormFactor};

    #[test]
    fn classifies_headphones_from_form_factor_and_name() {
        assert_eq!(
            classify_device(Headphones, "Realtek(R) Audio"),
            (DeviceKind::Headphones, false)
        );
        assert_eq!(
            classify_device(Headset, "USB Audio Device"),
            (DeviceKind::Headphones, false)
        );
        assert_eq!(
            classify_device(Speakers, "Headphones (HyperX Cloud)"),
            (DeviceKind::Headphones, false)
        );
        assert_eq!(
            classify_device(UnknownFormFactor, "Наушники (AirPods Pro)"),
            (DeviceKind::Headphones, false)
        );
    }

    #[test]
    fn classifies_speakers_and_display_outputs() {
        assert_eq!(
            classify_device(Speakers, "Speakers (Realtek High Definition Audio)"),
            (DeviceKind::Speakers, false)
        );
        assert_eq!(
            classify_device(DigitalAudioDisplayDevice, "LG ULTRAGEAR (NVIDIA High Definition Audio)"),
            (DeviceKind::Speakers, true)
        );
    }

    #[test]
    fn win32_mmdevice_enumeration_smoke_check() {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if let Ok(mut mon) = AudioMonitor::new(HWND::default()) {
                let snap = mon.snapshot();
                for d in &snap.devices {
                    assert!(!d.id.is_empty());
                    assert!(!d.name.is_empty());
                }
                assert!(snap.volume_percent <= 100);
            }
            CoUninitialize();
        }
    }
}

