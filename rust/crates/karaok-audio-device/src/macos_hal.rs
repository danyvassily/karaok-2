//! Direct macOS Core Audio HAL + AUHAL implementation (§5, ADR-0002).
//!
//! Safety & memory invariants:
//! - Every `CFStringRef` obtained via `AudioObjectGetPropertyData` is released via `CFRelease`.
//! - Every `AudioObjectAddPropertyListener` is symmetrically removed on drop before dropping the context box.
//! - Every AUHAL stream stops, uninitializes, and disposes the `AudioUnit` BEFORE dropping the RT processor box,
//!   guaranteeing zero use-after-free on audio render/input threads!
//! - Zero heap allocations and zero locks inside AUHAL RT render/input callbacks.

use crate::{ActiveStreamHandle, AudioHalBackend, HalChangeSignal, HalSubscription};
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringGetCString, CFStringGetLength,
    CFStringGetMaximumSizeForEncoding, CFStringRef,
};
use coreaudio_sys::*;
use crossbeam_channel::{Receiver, Sender};
use karaok_audio_engine::{RtMicInputHandle, RtOutputProcessor};
use karaok_types::{
    redact_device_uid, ActiveStreamFormat, AudioDeviceInfo, ErrorCode, KaraokError, KaraokResult,
    TransportType,
};
use std::ffi::CStr;
use std::mem::{offset_of, size_of, zeroed};
use std::os::raw::{c_char, c_void};
use std::ptr::{addr_of, null, null_mut};
use std::sync::{Arc, Mutex};

pub struct MacOsCoreAudioBackend;

impl MacOsCoreAudioBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacOsCoreAudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn cfstring_to_rust(cf_str: CFStringRef) -> Option<String> {
    if cf_str.is_null() {
        return None;
    }
    unsafe {
        let len = CFStringGetLength(cf_str);
        let max_size = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        if max_size <= 0 {
            return Some(String::new());
        }
        let mut buf = vec![0u8; max_size as usize];
        let ok = CFStringGetCString(
            cf_str,
            buf.as_mut_ptr() as *mut c_char,
            max_size,
            kCFStringEncodingUTF8,
        );
        if ok != 0 {
            let cstr = CStr::from_ptr(buf.as_ptr() as *const c_char);
            Some(cstr.to_string_lossy().into_owned())
        } else {
            None
        }
    }
}

fn get_cfstring_property(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<String> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        if AudioObjectHasProperty(object_id, &addr) == 0 {
            return None;
        }
        let mut cf_str: CFStringRef = null();
        let mut data_size = size_of::<CFStringRef>() as u32;
        let status = AudioObjectGetPropertyData(
            object_id,
            &addr,
            0,
            null(),
            &mut data_size,
            &mut cf_str as *mut CFStringRef as *mut c_void,
        );
        if status != 0 || cf_str.is_null() {
            return None;
        }
        let out = cfstring_to_rust(cf_str);
        CFRelease(cf_str as CFTypeRef);
        out
    }
}

fn get_u32_property(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<u32> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        if AudioObjectHasProperty(object_id, &addr) == 0 {
            return None;
        }
        let mut val: u32 = 0;
        let mut size = size_of::<u32>() as u32;
        let status = AudioObjectGetPropertyData(
            object_id,
            &addr,
            0,
            null(),
            &mut size,
            &mut val as *mut u32 as *mut c_void,
        );
        if status == 0 {
            Some(val)
        } else {
            None
        }
    }
}

fn get_f64_property(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<f64> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        if AudioObjectHasProperty(object_id, &addr) == 0 {
            return None;
        }
        let mut val: f64 = 0.0;
        let mut size = size_of::<f64>() as u32;
        let status = AudioObjectGetPropertyData(
            object_id,
            &addr,
            0,
            null(),
            &mut size,
            &mut val as *mut f64 as *mut c_void,
        );
        if status == 0 && val.is_finite() && val > 0.0 {
            Some(val)
        } else {
            None
        }
    }
}

fn get_channel_count(device_id: AudioDeviceID, scope: AudioObjectPropertyScope) -> u32 {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(device_id, &addr, 0, null(), &mut size) != 0
            || (size as usize) < size_of::<AudioBufferList>()
        {
            return 0;
        }
        // 8-byte aligned storage for AudioBufferList (fixes alignment UB - B3)
        let u64_len = (size as usize).div_ceil(size_of::<u64>());
        let mut aligned = vec![0u64; u64_len];
        let mut out_size = size;
        let status = AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            &mut out_size,
            aligned.as_mut_ptr() as *mut c_void,
        );
        if status != 0 || (out_size as usize) < offset_of!(AudioBufferList, mBuffers) {
            return 0;
        }
        let abl_ptr = aligned.as_ptr() as *const AudioBufferList;
        let count = (*abl_ptr).mNumberBuffers as usize;
        let required_bytes = offset_of!(AudioBufferList, mBuffers)
            .saturating_add(count.saturating_mul(size_of::<AudioBuffer>()));
        if count == 0 || required_bytes > (out_size as usize) || required_bytes > aligned.len() * size_of::<u64>() {
            return 0;
        }
        let buffers_base = addr_of!((*abl_ptr).mBuffers) as *const AudioBuffer;
        let mut channels = 0u32;
        for i in 0..count {
            channels = channels.saturating_add((*buffers_base.add(i)).mNumberChannels);
        }
        channels
    }
}

fn get_available_sample_rates(device_id: AudioDeviceID) -> Vec<(f64, f64)> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyAvailableNominalSampleRates,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(device_id, &addr, 0, null(), &mut size) != 0 || size == 0 {
            return Vec::new();
        }
        let count = size as usize / size_of::<AudioValueRange>();
        let mut ranges = vec![
            AudioValueRange {
                mMinimum: 0.0,
                mMaximum: 0.0,
            };
            count
        ];
        if AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            &mut size,
            ranges.as_mut_ptr() as *mut c_void,
        ) != 0
        {
            return Vec::new();
        }
        ranges
            .into_iter()
            .filter(|r| r.mMinimum.is_finite() && r.mMaximum.is_finite() && r.mMaximum > 0.0)
            .map(|r| (r.mMinimum, r.mMaximum))
            .collect()
    }
}

fn get_buffer_frame_size_range(device_id: AudioDeviceID, scope: AudioObjectPropertyScope) -> Option<(u32, u32)> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyBufferFrameSizeRange,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        if AudioObjectHasProperty(device_id, &addr) == 0 {
            return None;
        }
        let mut range = AudioValueRange {
            mMinimum: 0.0,
            mMaximum: 0.0,
        };
        let mut size = size_of::<AudioValueRange>() as u32;
        if AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            null(),
            &mut size,
            &mut range as *mut AudioValueRange as *mut c_void,
        ) == 0
            && range.mMinimum > 0.0
            && range.mMaximum >= range.mMinimum
        {
            Some((range.mMinimum as u32, range.mMaximum as u32))
        } else {
            None
        }
    }
}

fn map_transport(raw: Option<u32>) -> TransportType {
    match raw {
        Some(x) if x == kAudioDeviceTransportTypeUSB => TransportType::Usb,
        Some(x)
            if x == kAudioDeviceTransportTypeBluetooth
                || x == kAudioDeviceTransportTypeBluetoothLE =>
        {
            TransportType::Bluetooth
        }
        Some(x) if x == kAudioDeviceTransportTypeBuiltIn => TransportType::BuiltIn,
        Some(x) if x == kAudioDeviceTransportTypeHDMI => TransportType::Hdmi,
        Some(x) if x == kAudioDeviceTransportTypeDisplayPort => TransportType::DisplayPort,
        Some(x) if x == kAudioDeviceTransportTypeVirtual => TransportType::Virtual,
        Some(x) if x == kAudioDeviceTransportTypeAggregate => TransportType::Aggregate,
        _ => TransportType::Unknown,
    }
}

fn find_device_id_by_uid(target_uid: &str) -> KaraokResult<(AudioDeviceID, AudioDeviceInfo)> {
    let backend = MacOsCoreAudioBackend::new();
    let devices = backend.enumerate_devices()?;
    let matched = devices
        .into_iter()
        .find(|d| d.uid == target_uid)
        .ok_or_else(|| {
            KaraokError::with_hint(
                ErrorCode::DeviceUnavailable,
                "Périphérique Core Audio introuvable pour l'UID sélectionné.",
                "Vérifiez la connexion du périphérique et rafraîchissez la liste dans Son.",
            )
        })?;

    // Resolve ephemeral AudioDeviceID freshly right before opening (never persisted!)
    let ids = list_hal_device_ids()?;
    for id in ids {
        if get_cfstring_property(id, kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal)
            .as_deref()
            == Some(target_uid)
        {
            return Ok((id, matched));
        }
    }
    Err(KaraokError::new(
        ErrorCode::DeviceUnavailable,
        "Le périphérique a disparu avant l'ouverture du flux.",
    ))
}

fn list_hal_device_ids() -> KaraokResult<Vec<AudioDeviceID>> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    unsafe {
        let mut size: u32 = 0;
        let st = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &addr, 0, null(), &mut size);
        if st != 0 {
            return Err(KaraokError::new(
                ErrorCode::InternalError,
                format!("Core Audio HAL : échec taille liste périphériques (OSStatus {st})."),
            ));
        }
        let count = size as usize / size_of::<AudioDeviceID>();
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut ids = vec![0 as AudioDeviceID; count];
        let st2 = AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &addr,
            0,
            null(),
            &mut size,
            ids.as_mut_ptr() as *mut c_void,
        );
        if st2 != 0 {
            return Err(KaraokError::new(
                ErrorCode::InternalError,
                format!("Core Audio HAL : échec lecture périphériques (OSStatus {st2})."),
            ));
        }
        Ok(ids)
    }
}

// ---- AUHAL Output Stream ----

struct AuhalOutputContext {
    processor: RtOutputProcessor,
}

pub struct MacOsOutputStream {
    unit: AudioComponentInstance,
    ctx_ptr: *mut AuhalOutputContext,
    format: ActiveStreamFormat,
}

unsafe impl Send for MacOsOutputStream {}

impl ActiveStreamHandle for MacOsOutputStream {
    fn format(&self) -> ActiveStreamFormat {
        self.format.clone()
    }
}

impl Drop for MacOsOutputStream {
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
            if !self.ctx_ptr.is_null() {
                drop(Box::from_raw(self.ctx_ptr));
                self.ctx_ptr = null_mut();
            }
        }
    }
}

unsafe extern "C" fn auhal_output_render_cb(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() || io_data.is_null() {
        return -50; // paramErr
    }
    let ctx = &mut *(in_ref_con as *mut AuhalOutputContext);
    let abl = &mut *io_data;
    if abl.mNumberBuffers == 0 {
        return 0;
    }
    let buf = &mut *abl.mBuffers.as_mut_ptr();
    if buf.mData.is_null() {
        return 0;
    }
    let sample_count = (buf.mDataByteSize as usize) / size_of::<f32>();
    let slice = std::slice::from_raw_parts_mut(buf.mData as *mut f32, sample_count);
    ctx.processor
        .render_stereo_interleaved(slice, in_number_frames as usize);
    0
}

// ---- AUHAL Input Stream ----

struct AuhalInputContext {
    unit: AudioComponentInstance,
    mic_handle: RtMicInputHandle,
    channels: u32,
    scratch: Vec<f32>, // preallocated outside RT callback!
}

pub struct MacOsInputStream {
    unit: AudioComponentInstance,
    ctx_ptr: *mut AuhalInputContext,
    format: ActiveStreamFormat,
    mic_return_slot: Arc<Mutex<Option<RtMicInputHandle>>>,
}

unsafe impl Send for MacOsInputStream {}

impl ActiveStreamHandle for MacOsInputStream {
    fn format(&self) -> ActiveStreamFormat {
        self.format.clone()
    }
}

impl Drop for MacOsInputStream {
    fn drop(&mut self) {
        unsafe {
            // Stop & dispose AUHAL BEFORE freeing ctx_ptr or returning RtMicInputHandle (B3, B6)
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
            if !self.ctx_ptr.is_null() {
                let ctx = Box::from_raw(self.ctx_ptr);
                self.ctx_ptr = null_mut();
                if let Ok(mut slot) = self.mic_return_slot.lock() {
                    *slot = Some(ctx.mic_handle);
                }
            }
        }
    }
}

unsafe extern "C" fn auhal_input_cb(
    in_ref_con: *mut c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    _io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() {
        return -50;
    }
    let ctx = &mut *(in_ref_con as *mut AuhalInputContext);
    let channels = ctx.channels.max(1) as usize;
    let needed_samples = (in_number_frames as usize).saturating_mul(channels);
    if needed_samples == 0 || needed_samples > ctx.scratch.len() {
        return 0;
    }
    let byte_len = (needed_samples * size_of::<f32>()) as u32;
    let mut abl = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [AudioBuffer {
            mNumberChannels: ctx.channels.max(1),
            mDataByteSize: byte_len,
            mData: ctx.scratch.as_mut_ptr() as *mut c_void,
        }],
    };
    let st = AudioUnitRender(
        ctx.unit,
        io_action_flags,
        in_time_stamp,
        in_bus_number,
        in_number_frames,
        &mut abl,
    );
    if st == 0 {
        ctx.mic_handle
            .push_interleaved(&ctx.scratch[..needed_samples], channels);
    }
    st
}

// ---- HAL Property Listeners (System + Dynamic Multi-Scope Per-Device — B8, P2-I2) ----

const DEVICE_PROPERTY_SPECS: [(AudioObjectPropertySelector, AudioObjectPropertyScope); 9] = [
    (kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal),
    (kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal),
    (kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal),
    (kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeGlobal),
    (kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyScopeInput),
    (kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyScopeOutput),
    (kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal),
    (kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyScopeInput),
    (kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyScopeOutput),
];

struct HalListenerGuard {
    tx_ptr: *mut Sender<HalChangeSignal>,
    /// Exact `(device_id, selector, scope)` listeners currently registered so hotplug add/remove & Drop stay symmetric (P2-I2).
    watched_entries: Vec<(AudioDeviceID, AudioObjectPropertySelector, AudioObjectPropertyScope)>,
}

unsafe impl Send for HalListenerGuard {}

impl HalListenerGuard {
    fn subscribe_device(&mut self, dev_id: AudioDeviceID) {
        if self.tx_ptr.is_null() {
            return;
        }
        unsafe {
            for (sel, scope) in DEVICE_PROPERTY_SPECS {
                if self.watched_entries.iter().any(|&(d, s, sc)| d == dev_id && s == sel && sc == scope) {
                    continue;
                }
                let addr = AudioObjectPropertyAddress {
                    mSelector: sel,
                    mScope: scope,
                    mElement: kAudioObjectPropertyElementMain,
                };
                if AudioObjectHasProperty(dev_id, &addr) != 0
                    && AudioObjectAddPropertyListener(
                        dev_id,
                        &addr,
                        Some(hal_property_listener_cb),
                        self.tx_ptr as *mut c_void,
                    ) == 0
                {
                    self.watched_entries.push((dev_id, sel, scope));
                }
            }
        }
    }

    fn unsubscribe_device(&mut self, dev_id: AudioDeviceID) {
        if self.tx_ptr.is_null() {
            return;
        }
        unsafe {
            self.watched_entries.retain(|&(d, sel, scope)| {
                if d == dev_id {
                    let addr = AudioObjectPropertyAddress {
                        mSelector: sel,
                        mScope: scope,
                        mElement: kAudioObjectPropertyElementMain,
                    };
                    AudioObjectRemovePropertyListener(
                        d,
                        &addr,
                        Some(hal_property_listener_cb),
                        self.tx_ptr as *mut c_void,
                    );
                    false
                } else {
                    true
                }
            });
        }
    }
}

impl HalSubscription for HalListenerGuard {
    fn sync_device_listeners(&mut self) {
        let current_ids = list_hal_device_ids().unwrap_or_default();
        // Prune removed devices first
        let mut stale_ids: Vec<AudioDeviceID> = self
            .watched_entries
            .iter()
            .map(|&(d, _, _)| d)
            .filter(|d| !current_ids.contains(d))
            .collect();
        stale_ids.sort_unstable();
        stale_ids.dedup();
        for stale in stale_ids {
            self.unsubscribe_device(stale);
        }
        // Subscribe newly hotplugged devices (e.g. Nothing Headphone (a) USB-C plugged after startup!)
        for dev_id in current_ids {
            if !self.watched_entries.iter().any(|&(d, _, _)| d == dev_id) {
                self.subscribe_device(dev_id);
            }
        }
    }
}

unsafe extern "C" fn hal_property_listener_cb(
    _in_object_id: AudioObjectID,
    in_number_addresses: u32,
    in_addresses: *const AudioObjectPropertyAddress,
    in_client_data: *mut c_void,
) -> OSStatus {
    if in_client_data.is_null() || in_addresses.is_null() {
        return 0;
    }
    let tx = &*(in_client_data as *const Sender<HalChangeSignal>);
    let addrs = std::slice::from_raw_parts(in_addresses, in_number_addresses as usize);
    for addr in addrs {
        let sig = match addr.mSelector {
            x if x == kAudioHardwarePropertyDevices => HalChangeSignal::DevicesListChanged,
            x if x == kAudioHardwarePropertyDefaultInputDevice => {
                HalChangeSignal::DefaultInputChanged
            }
            x if x == kAudioHardwarePropertyDefaultOutputDevice => {
                HalChangeSignal::DefaultOutputChanged
            }
            _ => HalChangeSignal::DevicePropertyChanged,
        };
        let _ = tx.try_send(sig);
    }
    0
}

impl Drop for HalListenerGuard {
    fn drop(&mut self) {
        let sys_selectors = [
            kAudioHardwarePropertyDevices,
            kAudioHardwarePropertyDefaultInputDevice,
            kAudioHardwarePropertyDefaultOutputDevice,
        ];
        unsafe {
            for sel in sys_selectors {
                let addr = AudioObjectPropertyAddress {
                    mSelector: sel,
                    mScope: kAudioObjectPropertyScopeGlobal,
                    mElement: kAudioObjectPropertyElementMain,
                };
                AudioObjectRemovePropertyListener(
                    kAudioObjectSystemObject,
                    &addr,
                    Some(hal_property_listener_cb),
                    self.tx_ptr as *mut c_void,
                );
            }
            for &(dev_id, sel, scope) in &self.watched_entries {
                let addr = AudioObjectPropertyAddress {
                    mSelector: sel,
                    mScope: scope,
                    mElement: kAudioObjectPropertyElementMain,
                };
                AudioObjectRemovePropertyListener(
                    dev_id,
                    &addr,
                    Some(hal_property_listener_cb),
                    self.tx_ptr as *mut c_void,
                );
            }
            self.watched_entries.clear();
            if !self.tx_ptr.is_null() {
                drop(Box::from_raw(self.tx_ptr));
                self.tx_ptr = null_mut();
            }
        }
    }
}

fn create_hal_audio_unit() -> KaraokResult<AudioComponentInstance> {
    let desc = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_HALOutput,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    unsafe {
        let comp = AudioComponentFindNext(null_mut(), &desc);
        if comp.is_null() {
            return Err(KaraokError::new(
                ErrorCode::RouteOpenFailed,
                "Composant AUHAL macOS introuvable.",
            ));
        }
        let mut unit: AudioComponentInstance = null_mut();
        let st = AudioComponentInstanceNew(comp, &mut unit);
        if st != 0 || unit.is_null() {
            return Err(KaraokError::new(
                ErrorCode::RouteOpenFailed,
                format!("Instanciation AUHAL échouée (OSStatus {st})."),
            ));
        }
        Ok(unit)
    }
}

impl AudioHalBackend for MacOsCoreAudioBackend {
    fn enumerate_devices(&self) -> KaraokResult<Vec<AudioDeviceInfo>> {
        let ids = list_hal_device_ids()?;
        let default_in = get_u32_property(
            kAudioObjectSystemObject,
            kAudioHardwarePropertyDefaultInputDevice,
            kAudioObjectPropertyScopeGlobal,
        );
        let default_out = get_u32_property(
            kAudioObjectSystemObject,
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        );

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(uid) =
                get_cfstring_property(id, kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal)
            else {
                continue;
            };
            let name = get_cfstring_property(
                id,
                kAudioObjectPropertyName,
                kAudioObjectPropertyScopeGlobal,
            )
            .unwrap_or_else(|| "Périphérique audio sans nom".to_string());
            let manufacturer = get_cfstring_property(
                id,
                kAudioObjectPropertyManufacturer,
                kAudioObjectPropertyScopeGlobal,
            );
            let transport = map_transport(get_u32_property(
                id,
                kAudioDevicePropertyTransportType,
                kAudioObjectPropertyScopeGlobal,
            ));

            let input_channels = get_channel_count(id, kAudioDevicePropertyScopeInput);
            let output_channels = get_channel_count(id, kAudioDevicePropertyScopeOutput);
            if input_channels == 0 && output_channels == 0 {
                continue;
            }

            let nominal_sample_rate_hz = get_f64_property(
                id,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            );
            let available_sample_rate_ranges_hz = get_available_sample_rates(id);

            let pref_scope = if output_channels > 0 {
                kAudioDevicePropertyScopeOutput
            } else {
                kAudioDevicePropertyScopeInput
            };
            let buffer_frame_size = get_u32_property(
                id,
                kAudioDevicePropertyBufferFrameSize,
                pref_scope,
            );
            let buffer_frame_size_range = get_buffer_frame_size_range(id, pref_scope);

            let input_latency_frames = if input_channels > 0 {
                get_u32_property(id, kAudioDevicePropertyLatency, kAudioDevicePropertyScopeInput)
            } else {
                None
            };
            let output_latency_frames = if output_channels > 0 {
                get_u32_property(id, kAudioDevicePropertyLatency, kAudioDevicePropertyScopeOutput)
            } else {
                None
            };
            let safety_offset_frames = get_u32_property(
                id,
                kAudioDevicePropertySafetyOffset,
                pref_scope,
            );
            let is_alive = get_u32_property(
                id,
                kAudioDevicePropertyDeviceIsAlive,
                kAudioObjectPropertyScopeGlobal,
            )
            .map(|v| v != 0)
            .unwrap_or(true);

            let nothing_candidate = AudioDeviceInfo::detect_nothing_headphone_a_candidate(
                &name,
                manufacturer.as_deref(),
                transport,
            );

            out.push(AudioDeviceInfo {
                redacted_uid: redact_device_uid(&uid),
                uid,
                name,
                manufacturer,
                transport,
                input_channels,
                output_channels,
                nominal_sample_rate_hz,
                available_sample_rate_ranges_hz,
                buffer_frame_size,
                buffer_frame_size_range,
                input_latency_frames,
                output_latency_frames,
                safety_offset_frames,
                is_alive,
                is_default_input: default_in == Some(id),
                is_default_output: default_out == Some(id),
                nothing_headphone_a_candidate: nothing_candidate,
            });
        }
        Ok(out)
    }

    fn open_output_stream(
        &self,
        device_uid: &str,
        processor: RtOutputProcessor,
    ) -> KaraokResult<Box<dyn ActiveStreamHandle>> {
        let (device_id, dev_info) = find_device_id_by_uid(device_uid)?;
        if !dev_info.has_output() {
            return Err(KaraokError::new(
                ErrorCode::RouteOpenFailed,
                "Le périphérique sélectionné ne dispose d'aucun canal de sortie.",
            ));
        }

        let unit = create_hal_audio_unit()?;
        let cleanup_unit = |u: AudioComponentInstance| unsafe {
            AudioComponentInstanceDispose(u);
        };

        unsafe {
            let enable_io: u32 = 1;
            let disable_io: u32 = 0;
            let st_out = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Output,
                0,
                &enable_io as *const u32 as *const c_void,
                size_of::<u32>() as u32,
            );
            let st_in = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Input,
                1,
                &disable_io as *const u32 as *const c_void,
                size_of::<u32>() as u32,
            );
            if st_out != 0 || st_in != 0 {
                cleanup_unit(unit);
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Configuration E/S sortie AUHAL échouée ({st_out}/{st_in})."),
                ));
            }

            let st_dev = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_CurrentDevice,
                kAudioUnitScope_Global,
                0,
                &device_id as *const AudioDeviceID as *const c_void,
                size_of::<AudioDeviceID>() as u32,
            );
            if st_dev != 0 {
                cleanup_unit(unit);
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Association du périphérique de sortie échouée (OSStatus {st_dev})."),
                ));
            }

            // Read actual device sample rate and configure stereo interleaved f32 client format
            let sr = dev_info.nominal_sample_rate_hz.unwrap_or(48_000.0);
            let client_asbd = AudioStreamBasicDescription {
                mSampleRate: sr,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            let st_fmt = AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                &client_asbd as *const AudioStreamBasicDescription as *const c_void,
                size_of::<AudioStreamBasicDescription>() as u32,
            );
            if st_fmt != 0 {
                cleanup_unit(unit);
                return Err(KaraokError::new(
                    ErrorCode::FormatUnsupported,
                    format!("Format PCM stéréo f32 refusé par la sortie (OSStatus {st_fmt})."),
                ));
            }

            let ctx = Box::new(AuhalOutputContext { processor });
            let ctx_ptr = Box::into_raw(ctx);
            let cb = AURenderCallbackStruct {
                inputProc: Some(auhal_output_render_cb),
                inputProcRefCon: ctx_ptr as *mut c_void,
            };
            let st_cb = AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                &cb as *const AURenderCallbackStruct as *const c_void,
                size_of::<AURenderCallbackStruct>() as u32,
            );
            if st_cb != 0 {
                // ALWAYS dispose unit BEFORE freeing ctx_ptr (B3)
                cleanup_unit(unit);
                drop(Box::from_raw(ctx_ptr));
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Enregistrement callback rendu échoué (OSStatus {st_cb})."),
                ));
            }

            let st_init = AudioUnitInitialize(unit);
            if st_init != 0 {
                cleanup_unit(unit);
                drop(Box::from_raw(ctx_ptr));
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Initialisation sortie AUHAL échouée (OSStatus {st_init})."),
                ));
            }
            let st_start = AudioOutputUnitStart(unit);
            if st_start != 0 {
                AudioUnitUninitialize(unit);
                cleanup_unit(unit);
                drop(Box::from_raw(ctx_ptr));
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Démarrage sortie AUHAL échoué (OSStatus {st_start})."),
                ));
            }

            // Re-read ACTUAL negotiated format after stream open (§5.2, §5.4)!
            let mut actual_asbd: AudioStreamBasicDescription = zeroed();
            let mut asbd_size = size_of::<AudioStreamBasicDescription>() as u32;
            let _ = AudioUnitGetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                0,
                &mut actual_asbd as *mut AudioStreamBasicDescription as *mut c_void,
                &mut asbd_size,
            );
            let active_sr = if actual_asbd.mSampleRate > 0.0 {
                actual_asbd.mSampleRate
            } else {
                sr
            };
            let active_buf = get_u32_property(
                device_id,
                kAudioDevicePropertyBufferFrameSize,
                kAudioDevicePropertyScopeOutput,
            )
            .unwrap_or(dev_info.buffer_frame_size.unwrap_or(512));

            let format = ActiveStreamFormat {
                device_name: dev_info.name,
                redacted_uid: dev_info.redacted_uid,
                transport: dev_info.transport,
                sample_rate_hz: active_sr,
                channels: if actual_asbd.mChannelsPerFrame > 0 {
                    actual_asbd.mChannelsPerFrame
                } else {
                    dev_info.output_channels
                },
                buffer_frames: active_buf,
                driver_latency_frames: get_u32_property(
                    device_id,
                    kAudioDevicePropertyLatency,
                    kAudioDevicePropertyScopeOutput,
                ),
                safety_offset_frames: get_u32_property(
                    device_id,
                    kAudioDevicePropertySafetyOffset,
                    kAudioDevicePropertyScopeOutput,
                ),
                is_simulated: false,
            };

            Ok(Box::new(MacOsOutputStream {
                unit,
                ctx_ptr,
                format,
            }))
        }
    }

    fn open_input_stream(
        &self,
        device_uid: &str,
        mic_handle: RtMicInputHandle,
        mic_return_slot: Arc<Mutex<Option<RtMicInputHandle>>>,
    ) -> KaraokResult<Box<dyn ActiveStreamHandle>> {
        let return_mic = |mh: RtMicInputHandle| {
            if let Ok(mut g) = mic_return_slot.lock() {
                *g = Some(mh);
            }
        };
        let (device_id, dev_info) = match find_device_id_by_uid(device_uid) {
            Ok(v) => v,
            Err(e) => {
                return_mic(mic_handle);
                return Err(e);
            }
        };
        if !dev_info.has_input() {
            return_mic(mic_handle);
            return Err(KaraokError::new(
                ErrorCode::RouteOpenFailed,
                "Le périphérique sélectionné n'expose aucun canal d'entrée microphone.",
            ));
        }

        let unit = match create_hal_audio_unit() {
            Ok(u) => u,
            Err(e) => {
                return_mic(mic_handle);
                return Err(e);
            }
        };
        let cleanup_unit = |u: AudioComponentInstance| unsafe {
            AudioComponentInstanceDispose(u);
        };

        unsafe {
            let enable_io: u32 = 1;
            let disable_io: u32 = 0;
            let st_in = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Input,
                1,
                &enable_io as *const u32 as *const c_void,
                size_of::<u32>() as u32,
            );
            let st_out = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Output,
                0,
                &disable_io as *const u32 as *const c_void,
                size_of::<u32>() as u32,
            );
            if st_in != 0 || st_out != 0 {
                cleanup_unit(unit);
                return_mic(mic_handle);
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Configuration E/S entrée AUHAL échouée ({st_in}/{st_out})."),
                ));
            }

            let st_dev = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_CurrentDevice,
                kAudioUnitScope_Global,
                0,
                &device_id as *const AudioDeviceID as *const c_void,
                size_of::<AudioDeviceID>() as u32,
            );
            if st_dev != 0 {
                cleanup_unit(unit);
                return_mic(mic_handle);
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Sélection explicite du micro échouée (OSStatus {st_dev})."),
                ));
            }

            let mut hw_asbd: AudioStreamBasicDescription = zeroed();
            let mut hw_size = size_of::<AudioStreamBasicDescription>() as u32;
            let _ = AudioUnitGetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                1,
                &mut hw_asbd as *mut AudioStreamBasicDescription as *mut c_void,
                &mut hw_size,
            );

            let active_sr = if hw_asbd.mSampleRate > 0.0 {
                hw_asbd.mSampleRate
            } else {
                dev_info.nominal_sample_rate_hz.unwrap_or(48_000.0)
            };
            let active_ch = if hw_asbd.mChannelsPerFrame > 0 {
                hw_asbd.mChannelsPerFrame
            } else {
                dev_info.input_channels.max(1)
            };

            let client_asbd = AudioStreamBasicDescription {
                mSampleRate: active_sr,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 4 * active_ch,
                mFramesPerPacket: 1,
                mBytesPerFrame: 4 * active_ch,
                mChannelsPerFrame: active_ch,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            let st_fmt = AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                1,
                &client_asbd as *const AudioStreamBasicDescription as *const c_void,
                size_of::<AudioStreamBasicDescription>() as u32,
            );
            if st_fmt != 0 {
                cleanup_unit(unit);
                return_mic(mic_handle);
                return Err(KaraokError::new(
                    ErrorCode::FormatUnsupported,
                    format!("Format capture PCM f32 refusé (OSStatus {st_fmt})."),
                ));
            }

            let scratch = vec![0.0_f32; 4096 * active_ch as usize];
            let ctx = Box::new(AuhalInputContext {
                unit,
                mic_handle,
                channels: active_ch,
                scratch,
            });
            let ctx_ptr = Box::into_raw(ctx);

            let cb = AURenderCallbackStruct {
                inputProc: Some(auhal_input_cb),
                inputProcRefCon: ctx_ptr as *mut c_void,
            };
            let st_cb = AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_SetInputCallback,
                kAudioUnitScope_Global,
                0,
                &cb as *const AURenderCallbackStruct as *const c_void,
                size_of::<AURenderCallbackStruct>() as u32,
            );
            if st_cb != 0 {
                // ALWAYS dispose unit BEFORE taking back ctx_ptr (B3, B6)
                cleanup_unit(unit);
                let reclaimed = Box::from_raw(ctx_ptr);
                return_mic(reclaimed.mic_handle);
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Enregistrement callback capture micro échoué (OSStatus {st_cb})."),
                ));
            }

            let st_init = AudioUnitInitialize(unit);
            if st_init != 0 {
                cleanup_unit(unit);
                let reclaimed = Box::from_raw(ctx_ptr);
                return_mic(reclaimed.mic_handle);
                // Honest error classification (I8): do not blame permission for every Core Audio init error
                return Err(KaraokError::with_hint(
                    ErrorCode::RouteOpenFailed,
                    format!("Initialisation capture micro échouée (OSStatus {st_init})."),
                    "Vérifiez que le micro est branché et autorisé dans Réglages Système > Confidentialité et sécurité > Microphone.",
                ));
            }
            let st_start = AudioOutputUnitStart(unit);
            if st_start != 0 {
                AudioUnitUninitialize(unit);
                cleanup_unit(unit);
                let reclaimed = Box::from_raw(ctx_ptr);
                return_mic(reclaimed.mic_handle);
                return Err(KaraokError::new(
                    ErrorCode::RouteOpenFailed,
                    format!("Démarrage capture micro échoué (OSStatus {st_start})."),
                ));
            }

            let active_buf = get_u32_property(
                device_id,
                kAudioDevicePropertyBufferFrameSize,
                kAudioDevicePropertyScopeInput,
            )
            .unwrap_or(dev_info.buffer_frame_size.unwrap_or(512));

            let format = ActiveStreamFormat {
                device_name: dev_info.name,
                redacted_uid: dev_info.redacted_uid,
                transport: dev_info.transport,
                sample_rate_hz: active_sr,
                channels: active_ch,
                buffer_frames: active_buf,
                driver_latency_frames: get_u32_property(
                    device_id,
                    kAudioDevicePropertyLatency,
                    kAudioDevicePropertyScopeInput,
                ),
                safety_offset_frames: get_u32_property(
                    device_id,
                    kAudioDevicePropertySafetyOffset,
                    kAudioDevicePropertyScopeInput,
                ),
                is_simulated: false,
            };

            Ok(Box::new(MacOsInputStream {
                unit,
                ctx_ptr,
                format,
                mic_return_slot,
            }))
        }
    }

    fn subscribe_changes(
        &self,
    ) -> KaraokResult<(Receiver<HalChangeSignal>, Box<dyn HalSubscription>)> {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let tx_ptr = Box::into_raw(Box::new(tx));
        let sys_selectors = [
            kAudioHardwarePropertyDevices,
            kAudioHardwarePropertyDefaultInputDevice,
            kAudioHardwarePropertyDefaultOutputDevice,
        ];
        unsafe {
            for sel in sys_selectors {
                let addr = AudioObjectPropertyAddress {
                    mSelector: sel,
                    mScope: kAudioObjectPropertyScopeGlobal,
                    mElement: kAudioObjectPropertyElementMain,
                };
                let _ = AudioObjectAddPropertyListener(
                    kAudioObjectSystemObject,
                    &addr,
                    Some(hal_property_listener_cb),
                    tx_ptr as *mut c_void,
                );
            }
        }
        let mut guard = HalListenerGuard {
            tx_ptr,
            watched_entries: Vec::new(),
        };
        guard.sync_device_listeners();
        Ok((rx, Box::new(guard)))
    }
}
