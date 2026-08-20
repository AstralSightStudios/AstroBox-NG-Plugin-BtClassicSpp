use anyhow::Result;
use dispatch::Queue;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, msg_send, ClassType, MainThreadMarker, MainThreadOnly, Message};
use objc2_foundation::{NSObject, NSString};
use objc2_io_bluetooth::{
    BluetoothRFCOMMChannelID, IOBluetoothDevice, IOBluetoothDeviceInquiry,
    IOBluetoothDeviceInquiryDelegate, IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate,
};
use objc2_io_kit::kIOReturnSuccess;
use once_cell::sync::Lazy;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::models::SPPDevice;

#[path = "audio_guard.rs"]
mod audio_guard;

/* ---------- Address format helpers ---------- */
/// Convert macOS-delivered address (typically with `-` separators) to the
/// canonical `XX:XX:XX:XX:XX:XX` form used across platforms.
fn normalize_addr_from_macos(raw: &str) -> String {
    raw.replace('-', ":").to_uppercase()
}

/// Convert canonical `XX:XX:XX:XX:XX:XX` address into the format expected by
/// macOS APIs (`XX-XX-XX-XX-XX-XX`).
fn addr_to_macos_format(addr: &str) -> String {
    addr.replace(':', "-").to_uppercase()
}

/* ---------- 把闭包封送到主线程并返回结果 ---------- */
fn run_on_main_thread<F, R>(f: F) -> R
where
    F: FnOnce(MainThreadMarker) -> R + Send + 'static,
    R: Send + 'static,
{
    if let Some(mtm) = MainThreadMarker::new() {
        return f(mtm);
    }
    let (tx, rx) = mpsc::channel();
    Queue::main().exec_sync(move || {
        let mtm = MainThreadMarker::new().expect("MainThreadMarker missing on main queue");
        let _ = tx.send(f(mtm));
    });
    rx.recv().unwrap()
}

/* ---------- 全局/线程局部状态 ---------- */
struct SharedState {
    scanned_devices: Vec<SPPDevice>,
    /// true = 连续扫描循环开启；false = stop_scan_impl 已请求停止
    scan_loop_running: bool,
    /// Connection state is keyed by the canonical device address.  Scanning is
    /// intentionally left global, but nothing below the scan state is global.
    connected_device_info: HashMap<String, SPPDevice>,
    on_connected_callbacks: HashMap<String, Box<dyn Fn() + Send + Sync + 'static>>,
    data_listener_callbacks:
        HashMap<String, Box<dyn FnMut(Result<Vec<u8>, String>) + Send + 'static>>,
}
impl SharedState {
    fn new() -> Self {
        Self {
            scanned_devices: Vec::new(),
            scan_loop_running: false,
            connected_device_info: HashMap::new(),
            on_connected_callbacks: HashMap::new(),
            data_listener_callbacks: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct MainThreadState {
    inquiry: Option<Retained<IOBluetoothDeviceInquiry>>,
    delegate: Option<Retained<BTDelegate>>,
    rfcomm_channels: HashMap<String, Retained<IOBluetoothRFCOMMChannel>>,
    /// The delegate only receives a channel.  Keep the reverse index so every
    /// callback is dispatched to the connection that owns that channel.
    channel_addrs: HashMap<usize, String>,
    pending_sends: HashMap<String, PendingRfcommSend>,
}

static SHARED_BT_STATE: Lazy<Arc<Mutex<SharedState>>> =
    Lazy::new(|| Arc::new(Mutex::new(SharedState::new())));

const RFCOMM_ASYNC_SEND_TIMEOUT: Duration = Duration::from_secs(45);
const RFCOMM_ASYNC_MAX_IN_FLIGHT: usize = 24;
const KIORETURN_BUSY: i32 = 0xE00002D5u32 as i32;
const KIORETURN_NOSPACE: i32 = 0xE00002DBu32 as i32;
const KIORETURN_UNDERRUN: i32 = 0xE00002E7u32 as i32;
const KIORETURN_OVERRUN: i32 = 0xE00002E8u32 as i32;

struct PendingRfcommSend {
    chunks: Vec<Vec<u8>>,
    next_chunk_idx: usize,
    in_flight: usize,
    completion_tx: Option<mpsc::Sender<Result<()>>>,
}

thread_local! {
    static MAIN_THREAD_STATE: RefCell<MainThreadState> =
        RefCell::new(MainThreadState::default());
}

fn is_retryable_rfcomm_backpressure(status: i32) -> bool {
    matches!(
        status,
        KIORETURN_BUSY | KIORETURN_NOSPACE | KIORETURN_UNDERRUN | KIORETURN_OVERRUN
    )
}

fn channel_key(chan: &IOBluetoothRFCOMMChannel) -> usize {
    chan as *const IOBluetoothRFCOMMChannel as usize
}

fn channel_addr(chan: &IOBluetoothRFCOMMChannel) -> Option<String> {
    MAIN_THREAD_STATE.with(|cell| cell.borrow().channel_addrs.get(&channel_key(chan)).cloned())
}

fn remove_channel_for_addr(addr: &str) -> Option<Retained<IOBluetoothRFCOMMChannel>> {
    MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let channel = state.rfcomm_channels.remove(addr);
        if let Some(chan) = channel.as_ref() {
            state.channel_addrs.remove(&channel_key(chan));
        }
        channel
    })
}

fn complete_pending_rfcomm_send(addr: &str, result: Result<()>) {
    let tx = MAIN_THREAD_STATE.with(|cell| {
        cell.borrow_mut()
            .pending_sends
            .remove(addr)
            .and_then(|mut pending| pending.completion_tx.take())
    });
    if let Some(tx) = tx {
        let _ = tx.send(result);
    }
}

fn cancel_pending_rfcomm_send(addr: &str) {
    MAIN_THREAD_STATE.with(|cell| {
        cell.borrow_mut().pending_sends.remove(addr);
    });
}

fn pump_pending_rfcomm_send(addr: &str, chan: &IOBluetoothRFCOMMChannel) {
    loop {
        enum Action {
            Continue,
            Wait,
            Complete,
            Fail(String),
            Noop,
        }

        let action = MAIN_THREAD_STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let Some(pending) = state.pending_sends.get_mut(addr) else {
                return Action::Noop;
            };

            if pending.in_flight >= RFCOMM_ASYNC_MAX_IN_FLIGHT {
                return Action::Wait;
            }

            if pending.next_chunk_idx >= pending.chunks.len() {
                if pending.in_flight == 0 {
                    return Action::Complete;
                }
                return Action::Wait;
            }

            if unsafe { chan.isTransmissionPaused() } {
                return Action::Wait;
            }

            let chunk = &pending.chunks[pending.next_chunk_idx];
            let ret = unsafe {
                chan.writeAsync_length_refcon(
                    chunk.as_ptr() as *mut c_void,
                    chunk.len() as u16,
                    std::ptr::null_mut(),
                )
            };

            if ret == kIOReturnSuccess {
                pending.next_chunk_idx += 1;
                pending.in_flight += 1;
                Action::Continue
            } else if is_retryable_rfcomm_backpressure(ret) {
                Action::Wait
            } else {
                let mtu = unsafe { chan.getMTU() as usize }.max(1);
                Action::Fail(format!(
                    "Failed to queue data via write_async, error code: {}, next_chunk_len={}, mtu={}, next_chunk_idx={}, total_chunks={}",
                    ret,
                    chunk.len(),
                    mtu,
                    pending.next_chunk_idx,
                    pending.chunks.len()
                ))
            }
        });

        match action {
            Action::Continue => continue,
            Action::Wait | Action::Noop => break,
            Action::Complete => {
                complete_pending_rfcomm_send(addr, Ok(()));
                break;
            }
            Action::Fail(err) => {
                complete_pending_rfcomm_send(addr, Err(corelib::anyhow_site!("{}", err)));
                break;
            }
        }
    }
}

/* ---------- Objective-C delegate ---------- */
define_class! {
    #[derive(Debug)]
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct BTDelegate;

    unsafe impl NSObjectProtocol for BTDelegate {}

    /* -- 设备发现回调 -- */
    unsafe impl IOBluetoothDeviceInquiryDelegate for BTDelegate {
        #[unsafe(method(deviceInquiryDeviceFound:device:))]
        fn device_inquiry_device_found_device(
            &self,
            _sender: &IOBluetoothDeviceInquiry,
            device:  &IOBluetoothDevice,
        ) {
            let raw_addr = unsafe { device.addressString() }
                .map(|s| s.to_string())
                .unwrap_or_default();
            let addr = normalize_addr_from_macos(&raw_addr);
            let name = unsafe { device.nameOrAddress() }.map(|s| s.to_string());

            let info = SPPDevice { name, address: addr.clone() };

            if let Ok(mut st) = SHARED_BT_STATE.lock() {
                if !st.scanned_devices.iter().any(|d| d.address == addr) {
                    st.scanned_devices.push(info);
                }
            }
        }

        /* -- 本轮 inquiry 结束 -- */
        #[unsafe(method(deviceInquiryComplete:error:aborted:))]
        fn device_inquiry_complete_error_aborted(
            &self,
            _sender: &IOBluetoothDeviceInquiry,
            _error:  i32,
            aborted: bool,
        ) {
            /* 1. 把 TLS 中的 inquiry 清掉，先释放 RefCell 借用 */
            MAIN_THREAD_STATE.with(|c| c.borrow_mut().inquiry = None);

            /* 2. 读取是否还要继续循环扫描 */
            let continue_loop = {
                if let Ok(st) = SHARED_BT_STATE.lock() {
                    st.scan_loop_running && !aborted
                } else {
                    false
                }
            };

            /* 3. 如果需要，再次启动新一轮 inquiry */
            if continue_loop {
                if let Err(e) = start_inquiry(self) {
                    eprintln!("Failed to restart Bluetooth inquiry: {:?}", e);
                    /* 出错就终止循环 */
                    if let Ok(mut st) = SHARED_BT_STATE.lock() {
                        st.scan_loop_running = false;
                    }
                }
            }
        }
    }

    /* -- RFCOMM 相关回调 -- */
    unsafe impl IOBluetoothRFCOMMChannelDelegate for BTDelegate {
        #[unsafe(method(rfcommChannelData:data:length:))]
        fn rfcomm_channel_data_data_length(
            &self,
            _chan: &IOBluetoothRFCOMMChannel,
            data:  *mut c_void,
            len:   usize,
        ) {
            let Some(addr) = channel_addr(_chan) else {
                log::warn!("Received RFCOMM data for an unknown channel");
                return;
            };
            let slice = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
            if let Ok(mut st) = SHARED_BT_STATE.lock() {
                if let Some(cb) = st.data_listener_callbacks.get_mut(&addr) {
                    cb(Ok(slice.to_vec()));
                }
            }
        }

        #[unsafe(method(rfcommChannelOpenComplete:status:))]
        fn rfcomm_channel_open_complete_status(
            &self,
            chan:   &IOBluetoothRFCOMMChannel,
            status: i32,
        ) {
            let Some(addr) = channel_addr(chan) else {
                log::warn!("RFCOMM open callback for an unknown channel");
                return;
            };

            if status == kIOReturnSuccess {
                MAIN_THREAD_STATE.with(|c| {
                    let mut state = c.borrow_mut();
                    state
                        .rfcomm_channels
                        .insert(addr.clone(), chan.retain());
                    state.channel_addrs.insert(channel_key(chan), addr.clone());
                });
                if let Ok(st) = SHARED_BT_STATE.lock() {
                    if let Some(cb) = st.on_connected_callbacks.get(&addr) {
                        cb();
                    }
                }
            } else {
                remove_channel_for_addr(&addr);
                complete_pending_rfcomm_send(
                    &addr,
                    Err(corelib::anyhow_site!("RFCOMM channel failed to open")),
                );
                audio_guard::stop_for(&addr);
                if let Ok(mut st) = SHARED_BT_STATE.lock() {
                    st.connected_device_info.remove(&addr);
                    st.on_connected_callbacks.remove(&addr);
                    if let Some(mut cb) = st.data_listener_callbacks.remove(&addr) {
                        cb(Err("Connection closed".into()));
                    }
                }
            }
        }

        #[unsafe(method(rfcommChannelClosed:))]
        fn rfcomm_channel_closed(&self, chan: &IOBluetoothRFCOMMChannel) {
            let addr = channel_addr(chan);
            unsafe {
                let _ = chan.closeChannel();
                if let Some(dev_retained) = chan.getDevice() {
                    let dev: &IOBluetoothDevice = &*dev_retained;
                    let _status: i32 = msg_send![dev, closeConnection];
                }
            }

            if let Some(addr) = addr {
                remove_channel_for_addr(&addr);
                complete_pending_rfcomm_send(
                    &addr,
                    Err(corelib::anyhow_site!(
                        "Connection closed during RFCOMM send"
                    )),
                );
                audio_guard::stop_for(&addr);
                if let Ok(mut st) = SHARED_BT_STATE.lock() {
                    st.connected_device_info.remove(&addr);
                    st.on_connected_callbacks.remove(&addr);
                    if let Some(mut cb) = st.data_listener_callbacks.remove(&addr) {
                        cb(Err("Connection closed".into()));
                    }
                }
                log::info!("Device {} disconnected", addr);
            }
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:))]
        fn rfcomm_channel_write_complete_refcon_status(
            &self,
            chan: &IOBluetoothRFCOMMChannel,
            _refcon: *mut c_void,
            status: i32,
        ) {
            let Some(addr) = channel_addr(chan) else {
                return;
            };
            if status != kIOReturnSuccess {
                complete_pending_rfcomm_send(
                    &addr,
                    Err(corelib::anyhow_site!(
                        "RFCOMM async write failed with status {}",
                        status
                    )),
                );
                return;
            }

            MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                if let Some(pending) = state.pending_sends.get_mut(&addr) {
                    pending.in_flight = pending.in_flight.saturating_sub(1);
                }
            });

            pump_pending_rfcomm_send(&addr, chan);
        }

        #[unsafe(method(rfcommChannelQueueSpaceAvailable:))]
        fn rfcomm_channel_queue_space_available(&self, chan: &IOBluetoothRFCOMMChannel) {
            if let Some(addr) = channel_addr(chan) {
                pump_pending_rfcomm_send(&addr, chan);
            }
        }

        #[unsafe(method(rfcommChannelFlowControlChanged:))]
        fn rfcomm_channel_flow_control_changed(&self, chan: &IOBluetoothRFCOMMChannel) {
            if !unsafe { chan.isTransmissionPaused() } {
                if let Some(addr) = channel_addr(chan) {
                    pump_pending_rfcomm_send(&addr, chan);
                }
            }
        }
    }
}

impl BTDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/* ---------- 辅助：启动一轮 inquiry ---------- */
fn start_inquiry(delegate: &BTDelegate) -> Result<()> {
    /* 新建对象 */
    let inquiry: Retained<IOBluetoothDeviceInquiry> =
        unsafe { IOBluetoothDeviceInquiry::inquiryWithDelegate(Some(delegate)) }
            .ok_or_else(|| corelib::anyhow_site!("Failed to create Bluetooth inquiry"))?;

    unsafe { inquiry.setUpdateNewDeviceNames(true) };
    let status = unsafe { inquiry.start() };
    if status != kIOReturnSuccess {
        corelib::bail_site!("Failed to start scan, error code: {}", status);
    }

    /* 保存到 TLS */
    MAIN_THREAD_STATE.with(|cell| cell.borrow_mut().inquiry = Some(inquiry));
    Ok(())
}

/* ---------- 对外跨平台接口 ---------- */
pub mod core {
    use objc2_io_bluetooth::{IOBluetoothSDPServiceRecord, IOBluetoothSDPUUID};

    use super::*;

    fn normalized_fallback_channels(fallback_channels: &[u8]) -> Vec<BluetoothRFCOMMChannelID> {
        let source: Vec<u8> = if fallback_channels.is_empty() {
            vec![5, 1]
        } else {
            fallback_channels.to_vec()
        };

        let mut out = Vec::new();
        for ch in source {
            let ch_id = ch as BluetoothRFCOMMChannelID;
            if ch_id != 0 && !out.contains(&ch_id) {
                out.push(ch_id);
            }
        }
        if out.is_empty() {
            out.extend([5, 1]);
        }
        out
    }

    fn get_or_create_delegate(mtm: MainThreadMarker) -> Result<Retained<BTDelegate>> {
        MAIN_THREAD_STATE.with(|cell| {
            let mut s = cell.borrow_mut();
            if s.delegate.is_none() {
                s.delegate = Some(BTDelegate::new(mtm));
            }
            Ok(s.delegate.as_ref().unwrap().clone())
        })
    }

    /* ---- 扫描 ---- */
    pub fn start_scan_impl() -> Result<()> {
        run_on_main_thread(|mtm| {
            let delegate = get_or_create_delegate(mtm)?;

            /* 如果已经在循环扫描，则先停再启，保持语义一致 */
            stop_scan_impl().ok();

            /* 清列表并标记循环开启 */
            if let Ok(mut st) = SHARED_BT_STATE.lock() {
                st.scanned_devices.clear();
                st.scan_loop_running = true;
            }

            /* 启动第一轮 inquiry */
            start_inquiry(&delegate)?;
            Ok(())
        })
    }

    pub fn stop_scan_impl() -> Result<()> {
        run_on_main_thread(|_mtm| {
            /* 标记循环停止 */
            if let Ok(mut st) = SHARED_BT_STATE.lock() {
                st.scan_loop_running = false;
            }

            /* 取出并停止当前 inquiry（若有） */
            let current = MAIN_THREAD_STATE.with(|c| c.borrow_mut().inquiry.take());
            if let Some(inquiry) = current {
                unsafe { inquiry.stop() }; // 触发 aborted=true 回调
            }
            Ok(())
        })
    }

    pub fn get_scanned_devices_impl() -> Result<Vec<SPPDevice>> {
        Ok(SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?
            .scanned_devices
            .clone())
    }

    /* ---------- 根据 SPP UUID 解析 RFCOMM Channel ---------- */
    fn rfcomm_channel_from_record(
        record: &IOBluetoothSDPServiceRecord,
    ) -> Option<BluetoothRFCOMMChannelID> {
        let mut ch: BluetoothRFCOMMChannelID = 0;
        if unsafe { record.getRFCOMMChannelID(&mut ch) } == kIOReturnSuccess && ch != 0 {
            Some(ch)
        } else {
            None
        }
    }

    fn resolve_spp_channel(device: &IOBluetoothDevice) -> Option<BluetoothRFCOMMChannelID> {
        // 0x1101 = Serial Port Profile UUID-16
        const SPP_UUID16: u16 = 0x1101;

        unsafe {
            let uuid_opt = IOBluetoothSDPUUID::uuid16(SPP_UUID16);
            let uuid = uuid_opt?;

            let cached_record: Option<Retained<IOBluetoothSDPServiceRecord>> =
                device.getServiceRecordForUUID(Some(&*uuid));
            if let Some(record) = cached_record {
                if let Some(ch) = rfcomm_channel_from_record(&record) {
                    log::info!("macOS SDP cache resolved SPP RFCOMM channel {}", ch);
                    return Some(ch);
                }
            }

            // IOBluetooth 的 SDP 查询是异步的；这里在短窗口内轮询查询结果，
            // 避免还没等到 service record 就直接落到硬编码 channel。
            let status = device.performSDPQuery(None);
            if status != kIOReturnSuccess {
                log::warn!(
                    "macOS SDP query failed to start for SPP UUID 0x1101: {}",
                    status
                );
                return None;
            }

            log::info!("macOS SDP query started for SPP UUID 0x1101; waiting for service record");
            for attempt in 1..=30 {
                std::thread::sleep(Duration::from_millis(100));
                let record_opt: Option<Retained<IOBluetoothSDPServiceRecord>> =
                    device.getServiceRecordForUUID(Some(&*uuid));
                if let Some(record) = record_opt {
                    if let Some(ch) = rfcomm_channel_from_record(&record) {
                        log::info!(
                            "macOS SDP resolved SPP RFCOMM channel {} after {}ms",
                            ch,
                            attempt * 100
                        );
                        return Some(ch);
                    }
                }
            }

            log::warn!("macOS SDP did not resolve SPP RFCOMM channel; using fallback channels");
            None
        }
    }

    /* ---- 连接 ---- */
    pub fn connect_impl(addr_str: &str, fallback_channels: &[u8]) -> Result<bool> {
        let addr = normalize_addr_from_macos(addr_str);
        let fallback_channels = normalized_fallback_channels(fallback_channels);
        run_on_main_thread(move |mtm| {
            stop_scan_impl().ok();
            // Tear down an old channel for this address without discarding
            // callbacks that were registered for the connection being opened.
            disconnect_addr_on_main_thread_preserving_callbacks(&addr).ok();

            /* ---- 找到目标设备 ---- */
            let dev_opt: Option<Retained<IOBluetoothDevice>> = {
                let api_addr = addr_to_macos_format(&addr);
                let addr_ns = NSString::from_str(&api_addr);
                unsafe {
                    msg_send![IOBluetoothDevice::class(), deviceWithAddressString: Some(&*addr_ns)]
                }
            };
            let dev =
                dev_opt.ok_or_else(|| corelib::anyhow_site!("Device not found for {}", addr))?;

            unsafe {
                let dev_ref: &IOBluetoothDevice = &*dev;
                let _status: i32 = msg_send![dev_ref, closeConnection];
            }

            let dev_name = unsafe { dev.nameOrAddress() }.map(|s| s.to_string());

            /* 连接前记录默认输出并启动 CoreAudio 守护：被手表抢占系统音频时切回。 */
            audio_guard::start(&addr, dev_name.clone());

            let delegate = get_or_create_delegate(mtm)?;

            /* 计算要尝试的 Channel 列表：SDP → 上层按设备厂商给出的 fallback */
            let mut try_channels: Vec<BluetoothRFCOMMChannelID> =
                resolve_spp_channel(&dev).into_iter().collect();
            for ch_id in &fallback_channels {
                if !try_channels.contains(ch_id) {
                    try_channels.push(*ch_id);
                }
            }
            log::info!(
                "macOS RFCOMM channel attempt order for {}: {:?}",
                addr,
                try_channels
            );

            let mut last_error = None;
            for ch_id in try_channels {
                let mut chan_opt: Option<Retained<IOBluetoothRFCOMMChannel>> = None;
                let status = unsafe {
                    dev.openRFCOMMChannelAsync_withChannelID_delegate(
                        Some(&mut chan_opt),
                        ch_id,
                        Some(&*delegate),
                    )
                };
                if status == kIOReturnSuccess {
                    if let Some(chan) = chan_opt {
                        let key = channel_key(&chan);
                        MAIN_THREAD_STATE.with(|cell| {
                            let mut state = cell.borrow_mut();
                            state.channel_addrs.insert(key, addr.clone());
                            state.rfcomm_channels.insert(addr.clone(), chan);
                        });
                    }

                    /* 提前写入 pending device，保持原 Windows 语义 */
                    let mut st = SHARED_BT_STATE.lock().map_err(|_| {
                        corelib::anyhow_site!("Failed to acquire Bluetooth state lock")
                    })?;
                    st.connected_device_info.insert(
                        addr.clone(),
                        SPPDevice {
                            name: dev_name.clone(),
                            address: addr.clone(),
                        },
                    );
                    log::info!("RFCOMM connect request sent on channel {}", ch_id);
                    return Ok(true);
                }
                last_error = Some(status);
                log::warn!("Channel {} rejected (err {})", ch_id, status);
            }

            /* 全部通道失败：停掉音频守护，避免悬留监听 */
            disconnect_addr_on_main_thread(&addr).ok();
            corelib::bail_site!("All RFCOMM channel attempts failed (last={:?})", last_error);
        })
    }

    pub fn get_connected_device_info_impl(addr: &str) -> Result<Option<SPPDevice>> {
        let addr = normalize_addr_from_macos(addr);
        Ok(SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?
            .connected_device_info
            .get(&addr)
            .cloned())
    }

    pub fn get_max_send_len_impl(addr: &str) -> Result<Option<usize>> {
        let addr = normalize_addr_from_macos(addr);
        run_on_main_thread(move |_| {
            MAIN_THREAD_STATE.with(|cell| {
                let state = cell.borrow();
                let Some(chan) = state.rfcomm_channels.get(&addr) else {
                    return Ok(None);
                };
                let mtu = unsafe { chan.getMTU() } as usize;
                Ok(Some(mtu.max(1)))
            })
        })
    }

    /* ---- 回调设置 ---- */
    pub fn on_connected_impl(addr: &str, cb: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()> {
        let addr = normalize_addr_from_macos(addr);
        SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?
            .on_connected_callbacks
            .insert(addr, cb);
        Ok(())
    }

    pub fn set_data_listener_impl(
        addr: &str,
        cb: Box<dyn FnMut(Result<Vec<u8>, String>) + Send + 'static>,
    ) -> Result<()> {
        let addr = normalize_addr_from_macos(addr);
        SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?
            .data_listener_callbacks
            .insert(addr, cb);
        Ok(())
    }

    pub fn start_subscription_impl(addr: &str) -> Result<()> {
        let _ = addr;
        /* macOS 的 IOBluetoothRFCOMMChannel 已自动回调数据，
        不需要额外线程，直接返回 OK */
        Ok(())
    }

    /* ---- 数据发送 & 断开 ---- */
    pub fn send_impl(addr: &str, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let addr = normalize_addr_from_macos(addr);
        let payload = data.to_vec();
        let (tx, rx) = mpsc::channel();
        let send_addr = addr.clone();

        run_on_main_thread(move |_mtm| {
            let chan = MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                let Some(chan) = state.rfcomm_channels.get(&send_addr).cloned() else {
                    return Err(corelib::anyhow_site!(
                        "Device not connected, cannot send data"
                    ));
                };
                if state.pending_sends.contains_key(&send_addr) {
                    return Err(corelib::anyhow_site!("RFCOMM send already in progress"));
                }

                let mtu = unsafe { chan.getMTU() as usize }.max(1);
                let chunks = payload
                    .chunks(mtu)
                    .map(|chunk| chunk.to_vec())
                    .collect::<Vec<_>>();

                state.pending_sends.insert(
                    send_addr.clone(),
                    PendingRfcommSend {
                        chunks,
                        next_chunk_idx: 0,
                        in_flight: 0,
                        completion_tx: Some(tx),
                    },
                );
                Ok(chan)
            })?;

            pump_pending_rfcomm_send(&send_addr, &chan);
            Ok::<(), anyhow::Error>(())
        })?;

        match rx.recv_timeout(RFCOMM_ASYNC_SEND_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                run_on_main_thread({
                    let addr = addr.clone();
                    move |_| cancel_pending_rfcomm_send(&addr)
                });
                Err(corelib::anyhow_site!(
                    "RFCOMM async send timed out after {:?}",
                    RFCOMM_ASYNC_SEND_TIMEOUT
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                run_on_main_thread({
                    let addr = addr.clone();
                    move |_| cancel_pending_rfcomm_send(&addr)
                });
                Err(corelib::anyhow_site!(
                    "RFCOMM async send completion channel disconnected"
                ))
            }
        }
    }

    fn disconnect_addr_on_main_thread_preserving_callbacks(addr: &str) -> Result<()> {
        disconnect_addr_on_main_thread_inner(addr, false)
    }

    fn disconnect_addr_on_main_thread(addr: &str) -> Result<()> {
        disconnect_addr_on_main_thread_inner(addr, true)
    }

    fn disconnect_addr_on_main_thread_inner(addr: &str, clear_callbacks: bool) -> Result<()> {
        audio_guard::stop_for(addr);
        let maybe_chan = remove_channel_for_addr(addr);
        complete_pending_rfcomm_send(
            addr,
            Err(corelib::anyhow_site!(
                "Connection disconnected during RFCOMM send"
            )),
        );
        if let Some(chan) = maybe_chan {
            let status = unsafe { chan.closeChannel() };
            unsafe {
                if let Some(dev_retained) = chan.getDevice() {
                    let dev: &IOBluetoothDevice = &*dev_retained;
                    let _status: i32 = msg_send![dev, closeConnection];
                }
            }
            if status != kIOReturnSuccess {
                eprintln!("Failed to close RFCOMM channel, error code: {}", status);
            }
        }
        let mut st = SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?;
        st.connected_device_info.remove(addr);
        if clear_callbacks {
            st.on_connected_callbacks.remove(addr);
            st.data_listener_callbacks.remove(addr);
        }
        Ok(())
    }

    pub fn disconnect_impl(addr: &str) -> Result<()> {
        let addr = normalize_addr_from_macos(addr);
        run_on_main_thread(move |_| disconnect_addr_on_main_thread(&addr))
    }

    pub fn disconnect_all_impl() -> Result<()> {
        run_on_main_thread(|_| {
            let mut addrs = {
                let st = SHARED_BT_STATE
                    .lock()
                    .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?;
                let mut addrs = st.connected_device_info.keys().cloned().collect::<Vec<_>>();
                for addr in st
                    .on_connected_callbacks
                    .keys()
                    .chain(st.data_listener_callbacks.keys())
                {
                    if !addrs.contains(addr) {
                        addrs.push(addr.clone());
                    }
                }
                addrs
            };
            MAIN_THREAD_STATE.with(|cell| {
                let state = cell.borrow();
                for addr in state
                    .rfcomm_channels
                    .keys()
                    .chain(state.pending_sends.keys())
                {
                    if !addrs.contains(addr) {
                        addrs.push(addr.clone());
                    }
                }
            });
            for addr in addrs {
                disconnect_addr_on_main_thread(&addr)?;
            }
            audio_guard::stop_all();
            Ok(())
        })
    }
}

/* ---------- 全局清理 --------- */
pub fn cleanup_bluetooth_resources() {
    let _ = core::disconnect_all_impl();
    let _ = core::stop_scan_impl();
}
