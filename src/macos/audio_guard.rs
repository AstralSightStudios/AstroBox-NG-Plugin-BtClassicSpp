//! CoreAudio 输出守卫（防止手表抢占系统音频输出）
//!
//! 即使我们尽量以「不强制配对」的方式连接（见 implementation.rs 中的 unbonded 尝试），
//! 一旦手表最终还是被系统当作已绑定的 HFP 音频设备，macOS 的 `bluetoothaudiod` 就会把
//! 系统默认输出切到手表上，抢走用户正在用的蓝牙耳机。
//!
//! IOBluetooth 没有任何「禁止把本设备当成音频设备」的公开接口，所以这里在 CoreAudio 层兜底：
//! 监听 `kAudioHardwarePropertyDefaultOutputDevice`，一旦默认输出被切到「手表」（按蓝牙 MAC
//! 命中其 CoreAudio UID，或退化为名称匹配），立即把默认输出切回连接前记录的设备。
//!
//! 全部 CoreAudio 调用都是线程安全的，回调对所在线程无要求；本模块的注册/注销由
//! 专用蓝牙线程发起（该线程跑着自己的 CFRunLoop），与主线程（UI）解耦。

use objc2_core_foundation::{CFRetained, CFString};
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

#[allow(non_camel_case_types)]
type OSStatus = i32;
#[allow(non_camel_case_types)]
type AudioObjectID = u32;
type AudioDeviceID = AudioObjectID;

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;

/// FourCharCode（big-endian 的 4 个 ASCII 字符）。
const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const K_PROP_DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
const K_PROP_DEVICE_UID: u32 = fourcc(b"uid "); // kAudioDevicePropertyDeviceUID
const K_PROP_TRANSPORT_TYPE: u32 = fourcc(b"tran"); // kAudioDevicePropertyTransportType
const K_PROP_OBJECT_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName
const K_SCOPE_GLOBAL: u32 = fourcc(b"glob"); // kAudioObjectPropertyScopeGlobal
const K_ELEMENT_MAIN: u32 = 0; // kAudioObjectPropertyElementMain
const K_TRANSPORT_BLUETOOTH: u32 = fourcc(b"blue"); // kAudioDeviceTransportTypeBluetooth
const K_TRANSPORT_BLUETOOTH_LE: u32 = fourcc(b"blea"); // kAudioDeviceTransportTypeBluetoothLE

#[repr(C)]
struct AudioObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

type AudioObjectPropertyListenerProc = unsafe extern "C" fn(
    in_object_id: AudioObjectID,
    in_number_addresses: u32,
    in_addresses: *const AudioObjectPropertyAddress,
    in_client_data: *mut c_void,
) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        in_data_size: u32,
        in_data: *const c_void,
    ) -> OSStatus;
    fn AudioObjectAddPropertyListener(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_listener: AudioObjectPropertyListenerProc,
        in_client_data: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectRemovePropertyListener(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_listener: AudioObjectPropertyListenerProc,
        in_client_data: *mut c_void,
    ) -> OSStatus;
}

struct GuardState {
    active: bool,
    /// 目标手表的 MAC（仅保留十六进制大写，无分隔符）。
    watch_mac_hex: String,
    /// 目标手表名称（UID 命中失败时退化匹配）。
    watch_name: Option<String>,
    /// 被抢占时要恢复到的设备（连接前记录的默认输出）。
    preferred: AudioDeviceID,
}

static GUARD: Lazy<Mutex<GuardState>> = Lazy::new(|| {
    Mutex::new(GuardState {
        active: false,
        watch_mac_hex: String::new(),
        watch_name: None,
        preferred: 0,
    })
});

fn prop_addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        m_selector: selector,
        m_scope: K_SCOPE_GLOBAL,
        m_element: K_ELEMENT_MAIN,
    }
}

/// 仅保留十六进制字符并转大写，用于跨格式（`AA:BB`、`AA-BB`、`aabb`）比较 MAC。
fn normalize_hex(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

fn get_default_output() -> AudioDeviceID {
    let addr = prop_addr(K_PROP_DEFAULT_OUTPUT_DEVICE);
    let mut dev: AudioDeviceID = 0;
    let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            ptr::null(),
            &mut size,
            &mut dev as *mut _ as *mut c_void,
        )
    };
    if status == 0 {
        dev
    } else {
        0
    }
}

fn set_default_output(dev: AudioDeviceID) -> bool {
    let addr = prop_addr(K_PROP_DEFAULT_OUTPUT_DEVICE);
    let status = unsafe {
        AudioObjectSetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            ptr::null(),
            std::mem::size_of::<AudioDeviceID>() as u32,
            &dev as *const _ as *const c_void,
        )
    };
    status == 0
}

fn device_transport(dev: AudioDeviceID) -> u32 {
    let addr = prop_addr(K_PROP_TRANSPORT_TYPE);
    let mut val: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            dev,
            &addr,
            0,
            ptr::null(),
            &mut size,
            &mut val as *mut _ as *mut c_void,
        )
    };
    if status == 0 {
        val
    } else {
        0
    }
}

/// 读取设备的 CFString 属性（UID / 名称），返回 Rust String。
fn device_string_prop(dev: AudioDeviceID, selector: u32) -> Option<String> {
    let addr = prop_addr(selector);
    let mut cfstr: *const CFString = ptr::null();
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            dev,
            &addr,
            0,
            ptr::null(),
            &mut size,
            &mut cfstr as *mut _ as *mut c_void,
        )
    };
    if status != 0 {
        return None;
    }
    let ptr = NonNull::new(cfstr as *mut CFString)?;
    // CoreAudio 的 Copy* 语义：返回 +1 引用，交给 CFRetained 在作用域结束时释放。
    let retained = unsafe { CFRetained::from_raw(ptr) };
    Some(retained.to_string())
}

fn device_uid(dev: AudioDeviceID) -> Option<String> {
    device_string_prop(dev, K_PROP_DEVICE_UID)
}

fn device_name(dev: AudioDeviceID) -> Option<String> {
    device_string_prop(dev, K_PROP_OBJECT_NAME)
}

/// 判断某个音频设备是否就是目标手表。
fn is_watch_device(dev: AudioDeviceID, mac_hex: &str, name: &Option<String>) -> bool {
    if dev == 0 {
        return false;
    }
    // 只有蓝牙传输的设备才可能是手表，先快速排除内建/USB 等。
    let transport = device_transport(dev);
    if transport != K_TRANSPORT_BLUETOOTH && transport != K_TRANSPORT_BLUETOOTH_LE {
        return false;
    }
    // 主匹配：MAC 命中 CoreAudio UID。
    if !mac_hex.is_empty() {
        if let Some(uid) = device_uid(dev) {
            if normalize_hex(&uid).contains(mac_hex) {
                return true;
            }
        }
    }
    // 退化匹配：蓝牙设备且名称完全一致。
    if let Some(target) = name.as_ref() {
        if !target.is_empty() {
            if let Some(dn) = device_name(dev) {
                if dn == *target {
                    return true;
                }
            }
        }
    }
    false
}

/// 默认输出变化回调：若被切到手表，立刻切回 preferred。
unsafe extern "C" fn default_output_changed(
    _in_object_id: AudioObjectID,
    _in_number_addresses: u32,
    _in_addresses: *const AudioObjectPropertyAddress,
    _in_client_data: *mut c_void,
) -> OSStatus {
    let (active, mac, name, preferred) = match GUARD.lock() {
        Ok(g) => (
            g.active,
            g.watch_mac_hex.clone(),
            g.watch_name.clone(),
            g.preferred,
        ),
        Err(_) => return 0,
    };
    if !active {
        return 0;
    }

    let current = get_default_output();
    if !is_watch_device(current, &mac, &name) {
        return 0;
    }

    if preferred != 0 && preferred != current && !is_watch_device(preferred, &mac, &name) {
        if set_default_output(preferred) {
            log::info!(
                "audio_guard: 手表抢占了默认输出，已切回设备 {}",
                preferred
            );
        } else {
            log::warn!("audio_guard: 尝试切回默认输出失败 (preferred={})", preferred);
        }
    } else {
        log::warn!("audio_guard: 手表抢占了默认输出，但没有可恢复的目标设备");
    }
    0
}

/// 开始守护：记录连接前的默认输出，并注册监听。可重复调用（更新目标）。
pub fn start(addr: &str, name: Option<String>) {
    let mac_hex = normalize_hex(addr);
    let current = get_default_output();
    let current_is_watch = is_watch_device(current, &mac_hex, &name);

    let need_register = {
        let mut g = match GUARD.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.watch_mac_hex = mac_hex.clone();
        g.watch_name = name;
        // 连接前的默认输出（通常是用户的耳机）作为恢复目标；若此刻已是手表则不覆盖。
        if !current_is_watch {
            g.preferred = current;
        }
        let need = !g.active;
        if need {
            g.active = true; // 先置位，避免注册成功后回调读到 active=false
        }
        need
    };

    if need_register {
        let addr = prop_addr(K_PROP_DEFAULT_OUTPUT_DEVICE);
        let status = unsafe {
            AudioObjectAddPropertyListener(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &addr,
                default_output_changed,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            if let Ok(mut g) = GUARD.lock() {
                g.active = false;
            }
            log::warn!(
                "audio_guard: AudioObjectAddPropertyListener 失败: {}",
                status
            );
        } else {
            log::info!("audio_guard: 已启动，监听默认输出是否被手表抢占");
        }
    }
}

/// 停止守护：注销监听并清空状态。
pub fn stop() {
    let was_active = match GUARD.lock() {
        Ok(mut g) => {
            let a = g.active;
            g.active = false;
            g.preferred = 0;
            g.watch_mac_hex.clear();
            g.watch_name = None;
            a
        }
        Err(_) => return,
    };

    if was_active {
        let addr = prop_addr(K_PROP_DEFAULT_OUTPUT_DEVICE);
        let status = unsafe {
            AudioObjectRemovePropertyListener(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &addr,
                default_output_changed,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            log::warn!(
                "audio_guard: AudioObjectRemovePropertyListener 失败: {}",
                status
            );
        } else {
            log::info!("audio_guard: 已停止");
        }
    }
}
