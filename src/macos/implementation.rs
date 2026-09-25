use anyhow::Result;
use dispatch::Queue;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSObject, NSString};
use objc2_io_bluetooth::{
    BluetoothRFCOMMChannelID, IOBluetoothDevice, IOBluetoothDeviceInquiry,
    IOBluetoothDeviceInquiryDelegate, IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate,
    IOBluetoothSDPServiceRecord, IOBluetoothSDPUUID,
};
use objc2_io_kit::kIOReturnSuccess;
use once_cell::sync::Lazy;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::models::SPPDevice;

#[path = "audio_guard.rs"]
mod audio_guard;

fn normalize_addr_from_macos(raw: &str) -> String {
    raw.replace('-', ":").to_uppercase()
}

fn addr_to_macos_format(addr: &str) -> String {
    addr.replace(':', "-").to_uppercase()
}

/* ---------- IOBluetooth 对象和回调始终留在主 run loop ---------- */
fn run_on_main_thread<F, R>(f: F) -> R
where
    F: FnOnce(MainThreadMarker) -> R + Send + 'static,
    R: Send + 'static,
{
    if let Some(mtm) = MainThreadMarker::new() {
        return f(mtm);
    }
    let (tx, rx) = mpsc::channel();
    Queue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("MainThreadMarker missing on main queue");
        let _ = tx.send(f(mtm));
    });
    rx.recv().expect("Bluetooth main queue task did not return")
}

type ConnectedCallback = Arc<dyn Fn() + Send + Sync + 'static>;

struct ReceivedData {
    active: Option<Arc<AtomicBool>>,
    data: Result<Vec<u8>, String>,
}

struct SharedState {
    scanned_devices: Vec<SPPDevice>,
    scan_loop_running: bool,
    connected_device_info: HashMap<String, SPPDevice>,
    on_connected_callbacks: HashMap<String, ConnectedCallback>,
    // Only enqueue owned bytes on the main thread. Each device's listener runs
    // on its own worker, in order, without holding the global Bluetooth lock.
    data_listeners: HashMap<String, mpsc::Sender<ReceivedData>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            scanned_devices: Vec::new(),
            scan_loop_running: false,
            connected_device_info: HashMap::new(),
            on_connected_callbacks: HashMap::new(),
            data_listeners: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct MainThreadState {
    inquiry: Option<Retained<IOBluetoothDeviceInquiry>>,
    delegate: Option<Retained<BTDelegate>>,
    sessions: HashMap<String, RfcommSession>,
    // A failed native close must not free buffers still owned by writeAsync.
    retiring: HashMap<usize, RfcommSession>,
    connections: HashMap<String, PendingConnection>,
    connect_queue: VecDeque<String>,
    // Serialize connection establishment, not established connections or sends.
    // SDP/pairing on the shared controller must not race another open attempt.
    connecting: Option<String>,
    next_id: usize,
}

struct PendingConnection {
    generation: usize,
    device: Retained<IOBluetoothDevice>,
    info: SPPDevice,
    channels: VecDeque<BluetoothRFCOMMChannelID>,
    last_error: String,
    sdp_resolved: bool,
}

struct RfcommSession {
    generation: usize,
    channel: Retained<IOBluetoothRFCOMMChannel>,
    // IOBluetooth does not retain delegates on modern macOS.
    _delegate: Retained<BTDelegate>,
    active: Arc<AtomicBool>,
    opened: bool,
    failed: bool,
    mtu: usize,
    pump_scheduled: bool,
    pending_send: Option<PendingRfcommSend>,
}

struct PendingRfcommSend {
    id: usize,
    payload: Arc<Vec<u8>>,
    next_offset: usize,
    in_flight: usize,
    completion_tx: mpsc::Sender<Result<()>>,
}

static SHARED_BT_STATE: Lazy<Mutex<SharedState>> = Lazy::new(|| Mutex::new(SharedState::new()));

const RFCOMM_ASYNC_SEND_TIMEOUT: Duration = Duration::from_secs(45);
const RFCOMM_OPEN_TIMEOUT: Duration = Duration::from_secs(12);
const RFCOMM_ASYNC_MAX_IN_FLIGHT: usize = 8;
const RFCOMM_WRITES_PER_TURN: usize = 4;
const RFCOMM_PUMP_BUDGET: Duration = Duration::from_millis(2);
const RFCOMM_PUMP_YIELD: Duration = Duration::from_millis(1);
const RFCOMM_BACKPRESSURE_RETRY: Duration = Duration::from_millis(10);
const KIORETURN_BUSY: i32 = 0xE00002D5u32 as i32;
const KIORETURN_NOSPACE: i32 = 0xE00002DBu32 as i32;
const KIORETURN_UNDERRUN: i32 = 0xE00002E7u32 as i32;
const KIORETURN_OVERRUN: i32 = 0xE00002E8u32 as i32;

thread_local! {
    static MAIN_THREAD_STATE: RefCell<MainThreadState> =
        RefCell::new(MainThreadState::default());
}

fn next_id() -> usize {
    MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        state.next_id = state.next_id.wrapping_add(1).max(1);
        state.next_id
    })
}

fn is_retryable_rfcomm_backpressure(status: i32) -> bool {
    matches!(
        status,
        KIORETURN_BUSY | KIORETURN_NOSPACE | KIORETURN_UNDERRUN | KIORETURN_OVERRUN
    )
}

fn session_matches(addr: &str, generation: usize) -> bool {
    MAIN_THREAD_STATE.with(|cell| {
        cell.borrow()
            .sessions
            .get(addr)
            .is_some_and(|s| s.generation == generation)
    })
}

fn notify_connected(addr: &str, active: Arc<AtomicBool>) {
    let callback = SHARED_BT_STATE
        .lock()
        .ok()
        .and_then(|s| s.on_connected_callbacks.get(addr).cloned());
    if let Some(callback) = callback {
        // This callback may synchronously call send(), which waits for a main
        // run loop write completion. Never invoke it on that same run loop.
        std::thread::spawn(move || {
            if active.load(Ordering::Acquire) {
                callback();
            }
        });
    }
}

fn clear_device_state(addr: &str, error: Option<String>) {
    let listener = if let Ok(mut state) = SHARED_BT_STATE.lock() {
        state.connected_device_info.remove(addr);
        state.on_connected_callbacks.remove(addr);
        state.data_listeners.remove(addr)
    } else {
        None
    };
    if let (Some(listener), Some(error)) = (listener, error) {
        let _ = listener.send(ReceivedData {
            active: None,
            data: Err(error),
        });
    }
}

fn release_retiring_session(generation: usize) {
    let session = MAIN_THREAD_STATE.with(|cell| cell.borrow_mut().retiring.remove(&generation));
    if let Some(session) = session {
        unsafe {
            let _ = session.channel.setDelegate(None);
        }
    }
}

fn close_session(addr: &str, reason: &str, already_closed: bool) {
    let session = MAIN_THREAD_STATE.with(|cell| cell.borrow_mut().sessions.remove(addr));
    let Some(session) = session else {
        return;
    };
    session.active.store(false, Ordering::Release);
    if let Some(pending) = session.pending_send.as_ref() {
        let _ = pending
            .completion_tx
            .send(Err(corelib::anyhow_site!("{}", reason)));
    }
    let generation = session.generation;
    let channel = session.channel.clone();
    // Invalidate the live generation BEFORE native calls (which can reenter).
    // Keep both the delegate and any in-flight payload through native teardown.
    MAIN_THREAD_STATE.with(|cell| {
        cell.borrow_mut().retiring.insert(generation, session);
    });
    let status = if already_closed {
        kIOReturnSuccess
    } else {
        unsafe { channel.closeChannel() }
    };
    // Never close the device ACL: it may carry other RFCOMM/audio services.
    if status == kIOReturnSuccess {
        release_retiring_session(generation);
    } else {
        log::warn!(
            "Failed to close RFCOMM channel for {}: {}; awaiting native completion",
            addr,
            status
        );
        let pending_writes = MAIN_THREAD_STATE.with(|cell| {
            cell.borrow()
                .retiring
                .get(&generation)
                .and_then(|s| s.pending_send.as_ref())
                .is_some_and(|p| p.in_flight != 0)
        });
        if !pending_writes {
            release_retiring_session(generation);
        }
    }
}

fn release_connection_slot(addr: &str) {
    MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        state.connections.remove(addr);
        state.connect_queue.retain(|queued| queued != addr);
        if state.connecting.as_deref() == Some(addr) {
            state.connecting = None;
        }
    });
    Queue::main().exec_async(start_next_connection);
}

fn fail_connection(addr: &str, reason: String) {
    log::warn!("Bluetooth connection failed for {}: {}", addr, reason);
    close_session(addr, &reason, false);
    audio_guard::stop_for(addr);
    clear_device_state(addr, Some(reason));
    release_connection_slot(addr);
}

fn fail_session_later(addr: &str, generation: usize, reason: String) {
    let should_close = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let Some(session) = state.sessions.get_mut(addr) else {
            return false;
        };
        if session.generation != generation || session.failed {
            return false;
        }
        session.failed = true;
        session.active.store(false, Ordering::Release);
        true
    });
    if should_close {
        let addr = addr.to_owned();
        Queue::main().exec_async(move || {
            if session_matches(&addr, generation) {
                fail_connection(&addr, reason);
            }
        });
    }
}

fn schedule_pump(addr: &str, generation: usize, delay: Duration) {
    let scheduled = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let Some(session) = state.sessions.get_mut(addr) else {
            return false;
        };
        if session.generation != generation
            || session.failed
            || session.pump_scheduled
            || session.pending_send.is_none()
        {
            return false;
        }
        session.pump_scheduled = true;
        true
    });
    if scheduled {
        let addr = addr.to_owned();
        Queue::main().exec_after(delay, move || {
            let current = MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                let Some(session) = state.sessions.get_mut(&addr) else {
                    return false;
                };
                if session.generation != generation {
                    return false;
                }
                session.pump_scheduled = false;
                true
            });
            if current {
                pump_pending_rfcomm_send(&addr, generation);
            }
        });
    }
}

fn pump_pending_rfcomm_send(addr: &str, generation: usize) {
    let started = Instant::now();
    for _ in 0..RFCOMM_WRITES_PER_TURN {
        let next = MAIN_THREAD_STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let session = state.sessions.get_mut(addr)?;
            if session.generation != generation || !session.opened || session.failed {
                return None;
            }
            let pending = session.pending_send.as_mut()?;
            if pending.in_flight >= RFCOMM_ASYNC_MAX_IN_FLIGHT
                || pending.next_offset == pending.payload.len()
            {
                return None;
            }
            let offset = pending.next_offset;
            let len = session.mtu.min(pending.payload.len() - offset);
            // Reserve before calling native code so an inline completion is safe.
            pending.next_offset += len;
            pending.in_flight += 1;
            Some((
                session.channel.clone(),
                pending.payload.clone(),
                pending.id,
                offset,
                len,
            ))
        });
        let Some((channel, payload, id, offset, len)) = next else {
            return;
        };

        // Do not hold a RefCell borrow or a global mutex over IOBluetooth calls.
        let status = unsafe {
            if channel.isTransmissionPaused() {
                KIORETURN_BUSY
            } else {
                channel.writeAsync_length_refcon(
                    payload.as_ptr().add(offset) as *mut c_void,
                    len as u16,
                    id as *mut c_void,
                )
            }
        };
        if status != kIOReturnSuccess {
            MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                if let Some(session) = state.sessions.get_mut(addr) {
                    if session.generation == generation {
                        if let Some(pending) = session.pending_send.as_mut().filter(|p| p.id == id)
                        {
                            pending.next_offset = offset;
                            pending.in_flight -= 1;
                        }
                    }
                }
            });
            if is_retryable_rfcomm_backpressure(status) {
                // Some busy/no-space returns do not produce a queue-space event.
                schedule_pump(addr, generation, RFCOMM_BACKPRESSURE_RETRY);
            } else {
                fail_session_later(
                    addr,
                    generation,
                    format!("RFCOMM writeAsync failed: {}", status),
                );
            }
            return;
        }
        if started.elapsed() >= RFCOMM_PUMP_BUDGET {
            break;
        }
    }
    // Coalesce write/queue/flow callbacks and yield to UI and the other device.
    schedule_pump(addr, generation, RFCOMM_PUMP_YIELD);
}

fn handle_write_complete(addr: &str, generation: usize, id: usize, status: i32) {
    let retired = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let session = state.retiring.get_mut(&generation)?;
        if let Some(pending) = session.pending_send.as_mut().filter(|p| p.id == id) {
            pending.in_flight = pending.in_flight.saturating_sub(1);
            Some(pending.in_flight == 0)
        } else {
            Some(false)
        }
    });
    if let Some(finished) = retired {
        if finished {
            Queue::main().exec_async(move || release_retiring_session(generation));
        }
        return;
    }
    let outcome = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let session = state.sessions.get_mut(addr)?;
        if session.generation != generation {
            return None;
        }
        let pending = session.pending_send.as_mut().filter(|p| p.id == id)?;
        // Account for failed writes too, and drain completions while teardown
        // is scheduled. Otherwise a failed close would retain an inflated count.
        pending.in_flight = pending.in_flight.saturating_sub(1);
        let complete = status == kIOReturnSuccess
            && !session.failed
            && pending.next_offset == pending.payload.len()
            && pending.in_flight == 0;
        let completed = if complete {
            session.pending_send.take()
        } else {
            None
        };
        Some((session.failed, completed))
    });
    let Some((failed, completed)) = outcome else {
        return;
    };
    if failed {
        return;
    }
    if status != kIOReturnSuccess {
        fail_session_later(
            addr,
            generation,
            format!("RFCOMM async write failed: {}", status),
        );
    } else if let Some(pending) = completed {
        let _ = pending.completion_tx.send(Ok(()));
    } else {
        schedule_pump(addr, generation, RFCOMM_PUMP_YIELD);
    }
}

fn deliver_data(addr: &str, generation: usize, data: Vec<u8>) {
    let active = MAIN_THREAD_STATE.with(|cell| {
        cell.borrow()
            .sessions
            .get(addr)
            .filter(|s| s.generation == generation && !s.failed)
            .map(|s| s.active.clone())
    });
    let Some(active) = active else {
        return;
    };
    let listener = SHARED_BT_STATE
        .lock()
        .ok()
        .and_then(|state| state.data_listeners.get(addr).cloned());
    if let Some(listener) = listener {
        let _ = listener.send(ReceivedData {
            active: Some(active),
            data: Ok(data),
        });
    }
}

/* ---------- 每次开通道拥有独立代次，迟到的旧回调不能碰新连接 ---------- */
#[derive(Debug)]
struct DelegateState {
    session: Option<(String, usize)>,
}

define_class! {
    #[derive(Debug)]
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateState]
    struct BTDelegate;

    unsafe impl NSObjectProtocol for BTDelegate {}

    unsafe impl IOBluetoothDeviceInquiryDelegate for BTDelegate {
        #[unsafe(method(deviceInquiryDeviceFound:device:))]
        fn device_inquiry_device_found_device(
            &self, sender: &IOBluetoothDeviceInquiry, device: &IOBluetoothDevice,
        ) {
            if !is_current_inquiry(sender) { return; }
            let raw_addr = unsafe { device.addressString() }.map(|s| s.to_string()).unwrap_or_default();
            let addr = normalize_addr_from_macos(&raw_addr);
            let name = unsafe { device.nameOrAddress() }.map(|s| s.to_string());
            if let Ok(mut state) = SHARED_BT_STATE.lock() {
                if !state.scanned_devices.iter().any(|d| d.address == addr) {
                    state.scanned_devices.push(SPPDevice { name, address: addr });
                }
            }
        }

        #[unsafe(method(deviceInquiryComplete:error:aborted:))]
        fn device_inquiry_complete_error_aborted(
            &self, sender: &IOBluetoothDeviceInquiry, error: i32, aborted: bool,
        ) {
            // A delayed completion from a stopped scan must not clear its successor.
            if !is_current_inquiry(sender) { return; }
            MAIN_THREAD_STATE.with(|cell| { cell.borrow_mut().inquiry.take(); });
            let continue_loop = SHARED_BT_STATE.lock()
                .map(|state| state.scan_loop_running && !aborted && error == kIOReturnSuccess)
                .unwrap_or(false);
            if continue_loop {
                Queue::main().exec_async(|| {
                    let running = SHARED_BT_STATE.lock().map(|s| s.scan_loop_running).unwrap_or(false);
                    if running {
                        let delegate = MAIN_THREAD_STATE.with(|cell| cell.borrow().delegate.clone());
                        if let Some(delegate) = delegate {
                            if let Err(error) = start_inquiry(&delegate) {
                                log::warn!("Failed to restart Bluetooth inquiry: {}", error);
                                if let Ok(mut state) = SHARED_BT_STATE.lock() { state.scan_loop_running = false; }
                            }
                        }
                    }
                });
            } else if let Ok(mut state) = SHARED_BT_STATE.lock() {
                state.scan_loop_running = false;
            }
        }
    }

    unsafe impl IOBluetoothRFCOMMChannelDelegate for BTDelegate {
        #[unsafe(method(rfcommChannelData:data:length:))]
        fn rfcomm_channel_data_data_length(
            &self, _chan: &IOBluetoothRFCOMMChannel, data: *mut c_void, len: usize,
        ) {
            let Some((addr, generation)) = self.ivars().session.as_ref() else { return; };
            if data.is_null() || len == 0 { return; }
            let data = unsafe { std::slice::from_raw_parts(data as *const u8, len) }.to_vec();
            let opened = MAIN_THREAD_STATE.with(|cell| {
                cell.borrow().sessions.get(addr).is_some_and(|s| s.generation == *generation && s.opened)
            });
            if opened {
                deliver_data(addr, *generation, data);
            } else {
                let addr = addr.clone();
                let generation = *generation;
                Queue::main().exec_async(move || deliver_data(&addr, generation, data));
            }
        }

        #[unsafe(method(rfcommChannelOpenComplete:status:))]
        fn rfcomm_channel_open_complete_status(&self, _chan: &IOBluetoothRFCOMMChannel, status: i32) {
            if let Some((addr, generation)) = self.ivars().session.clone() {
                // openRFCOMMChannelAsync may callback before returning its channel.
                Queue::main().exec_async(move || handle_open_complete(&addr, generation, status));
            }
        }

        #[unsafe(method(rfcommChannelClosed:))]
        fn rfcomm_channel_closed(&self, _chan: &IOBluetoothRFCOMMChannel) {
            if let Some((addr, generation)) = self.ivars().session.clone() {
                Queue::main().exec_async(move || {
                    release_retiring_session(generation);
                    if !session_matches(&addr, generation) { return; }
                    close_session(&addr, "RFCOMM channel closed", true);
                    let opening = MAIN_THREAD_STATE.with(|cell| cell.borrow().connections.contains_key(&addr));
                    if opening {
                        retry_connection(&addr, "RFCOMM channel closed while opening".into());
                    } else {
                        audio_guard::stop_for(&addr);
                        clear_device_state(&addr, Some("Connection closed".into()));
                        log::info!("Device {} disconnected", addr);
                    }
                });
            }
        }

        #[unsafe(method(rfcommChannelWriteComplete:refcon:status:))]
        fn rfcomm_channel_write_complete_refcon_status(
            &self, _chan: &IOBluetoothRFCOMMChannel, refcon: *mut c_void, status: i32,
        ) {
            if let Some((addr, generation)) = self.ivars().session.as_ref() {
                handle_write_complete(addr, *generation, refcon as usize, status);
            }
        }

        #[unsafe(method(rfcommChannelQueueSpaceAvailable:))]
        fn rfcomm_channel_queue_space_available(&self, _chan: &IOBluetoothRFCOMMChannel) {
            if let Some((addr, generation)) = self.ivars().session.as_ref() {
                schedule_pump(addr, *generation, RFCOMM_PUMP_YIELD);
            }
        }

        #[unsafe(method(rfcommChannelFlowControlChanged:))]
        fn rfcomm_channel_flow_control_changed(&self, _chan: &IOBluetoothRFCOMMChannel) {
            if let Some((addr, generation)) = self.ivars().session.as_ref() {
                schedule_pump(addr, *generation, RFCOMM_PUMP_YIELD);
            }
        }
    }
}

impl BTDelegate {
    fn new(mtm: MainThreadMarker, session: Option<(String, usize)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateState { session });
        unsafe { msg_send![super(this), init] }
    }
}

fn is_current_inquiry(inquiry: &IOBluetoothDeviceInquiry) -> bool {
    MAIN_THREAD_STATE.with(|cell| {
        cell.borrow()
            .inquiry
            .as_ref()
            .is_some_and(|current| std::ptr::eq(&**current, inquiry))
    })
}

fn start_inquiry(delegate: &BTDelegate) -> Result<()> {
    if MAIN_THREAD_STATE.with(|cell| cell.borrow().inquiry.is_some()) {
        return Ok(());
    }
    let inquiry = unsafe { IOBluetoothDeviceInquiry::inquiryWithDelegate(Some(delegate)) }
        .ok_or_else(|| corelib::anyhow_site!("Failed to create Bluetooth inquiry"))?;
    unsafe { inquiry.setUpdateNewDeviceNames(true) };
    MAIN_THREAD_STATE.with(|cell| cell.borrow_mut().inquiry = Some(inquiry.clone()));
    let status = unsafe { inquiry.start() };
    if status != kIOReturnSuccess {
        if is_current_inquiry(&inquiry) {
            MAIN_THREAD_STATE.with(|cell| {
                cell.borrow_mut().inquiry.take();
            });
        }
        corelib::bail_site!("Failed to start scan, error code: {}", status);
    }
    Ok(())
}

/* ---------- 非阻塞 SDP / 通道回退 / 多设备连接排队 ---------- */
fn cached_spp_channel(device: &IOBluetoothDevice) -> Option<BluetoothRFCOMMChannelID> {
    unsafe {
        let uuid = IOBluetoothSDPUUID::uuid16(0x1101)?;
        let record: Retained<IOBluetoothSDPServiceRecord> =
            device.getServiceRecordForUUID(Some(&uuid))?;
        let mut channel = 0;
        if record.getRFCOMMChannelID(&mut channel) == kIOReturnSuccess
            && (1..=30).contains(&channel)
        {
            Some(channel)
        } else {
            None
        }
    }
}

fn start_next_connection() {
    let next = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        if state.connecting.is_some() {
            return None;
        }
        while let Some(addr) = state.connect_queue.pop_front() {
            if let Some(connection) = state.connections.get(&addr) {
                let result = (
                    addr.clone(),
                    connection.generation,
                    connection.device.clone(),
                    connection.info.name.clone(),
                    connection.sdp_resolved,
                );
                state.connecting = Some(addr);
                return Some(result);
            }
        }
        None
    });
    let Some((addr, generation, device, name, sdp_resolved)) = next else {
        return;
    };
    if sdp_resolved {
        try_next_channel(&addr);
        return;
    }
    audio_guard::start(&addr, name);
    if let Some(channel) = cached_spp_channel(&device) {
        finish_sdp(&addr, generation, Some(channel));
        return;
    }
    let status = unsafe { device.performSDPQuery(None) };
    if status != kIOReturnSuccess {
        log::warn!("macOS SDP query failed to start for {}: {}", addr, status);
        finish_sdp(&addr, generation, None);
        return;
    }
    poll_sdp(addr, generation, 0);
}

fn poll_sdp(addr: String, generation: usize, attempt: usize) {
    Queue::main().exec_after(Duration::from_millis(100), move || {
        let device = MAIN_THREAD_STATE.with(|cell| {
            let state = cell.borrow();
            state
                .connections
                .get(&addr)
                .filter(|c| {
                    c.generation == generation && state.connecting.as_deref() == Some(addr.as_str())
                })
                .map(|c| c.device.clone())
        });
        let Some(device) = device else {
            return;
        };
        if let Some(channel) = cached_spp_channel(&device) {
            finish_sdp(&addr, generation, Some(channel));
        } else if attempt >= 29 {
            log::warn!(
                "SDP did not resolve SPP for {}; using fallback channels",
                addr
            );
            finish_sdp(&addr, generation, None);
        } else {
            poll_sdp(addr, generation, attempt + 1);
        }
    });
}

fn finish_sdp(addr: &str, generation: usize, channel: Option<BluetoothRFCOMMChannelID>) {
    let current = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let Some(connection) = state.connections.get_mut(addr) else {
            return false;
        };
        if connection.generation != generation {
            return false;
        }
        connection.sdp_resolved = true;
        if let Some(channel) = channel {
            connection.channels.retain(|ch| *ch != channel);
            connection.channels.push_front(channel);
        }
        true
    });
    if current {
        try_next_channel(addr);
    }
}

fn retry_connection(addr: &str, reason: String) {
    close_session(addr, &reason, false);
    let exhausted = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let connection = state.connections.get_mut(addr)?;
        connection.last_error = reason.clone();
        if connection.channels.is_empty() {
            return Some(true);
        }
        // Let the other device try before retrying an unreachable one.
        if state.connecting.as_deref() == Some(addr) {
            state.connecting = None;
        }
        state.connect_queue.push_back(addr.to_owned());
        Some(false)
    });
    match exhausted {
        Some(true) => fail_connection(
            addr,
            format!("All RFCOMM channel attempts failed: {}", reason),
        ),
        Some(false) => Queue::main().exec_async(start_next_connection),
        None => (),
    }
}

fn try_next_channel(addr: &str) {
    close_session(addr, "Retrying RFCOMM channel", false);
    let next = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let connection = state.connections.get_mut(addr)?;
        Some((
            connection.device.clone(),
            connection.channels.pop_front(),
            connection.last_error.clone(),
        ))
    });
    let Some((device, channel_id, last_error)) = next else {
        return;
    };
    let Some(channel_id) = channel_id else {
        fail_connection(
            addr,
            format!("All RFCOMM channel attempts failed: {}", last_error),
        );
        return;
    };
    let generation = next_id();
    let mtm = MainThreadMarker::new().expect("RFCOMM open must run on main thread");
    let delegate = BTDelegate::new(mtm, Some((addr.to_owned(), generation)));
    let mut channel = None;
    let status = unsafe {
        device.openRFCOMMChannelAsync_withChannelID_delegate(
            Some(&mut channel),
            channel_id,
            Some(&delegate),
        )
    };
    if status != kIOReturnSuccess || channel.is_none() {
        if let Some(channel) = channel {
            unsafe {
                let _ = channel.closeChannel();
                let _ = channel.setDelegate(None);
            }
        }
        let addr = addr.to_owned();
        let connection_generation = MAIN_THREAD_STATE
            .with(|cell| cell.borrow().connections.get(&addr).map(|c| c.generation));
        Queue::main().exec_async(move || {
            let current = MAIN_THREAD_STATE
                .with(|cell| cell.borrow().connections.get(&addr).map(|c| c.generation));
            if current.is_some() && current == connection_generation {
                retry_connection(
                    &addr,
                    format!("Channel {} rejected ({})", channel_id, status),
                );
            }
        });
        return;
    }
    let channel = channel.unwrap();
    let already_open = unsafe { channel.isOpen() };
    MAIN_THREAD_STATE.with(|cell| {
        cell.borrow_mut().sessions.insert(
            addr.to_owned(),
            RfcommSession {
                generation,
                channel,
                _delegate: delegate,
                active: Arc::new(AtomicBool::new(true)),
                opened: false,
                failed: false,
                mtu: 1,
                pump_scheduled: false,
                pending_send: None,
            },
        );
    });
    log::info!("RFCOMM open request for {} on channel {}", addr, channel_id);
    if already_open {
        handle_open_complete(addr, generation, kIOReturnSuccess);
    }
    let addr = addr.to_owned();
    Queue::main().exec_after(RFCOMM_OPEN_TIMEOUT, move || {
        let timed_out = MAIN_THREAD_STATE.with(|cell| {
            cell.borrow()
                .sessions
                .get(&addr)
                .is_some_and(|s| s.generation == generation && !s.opened)
        });
        if timed_out {
            retry_connection(&addr, format!("Channel {} open timed out", channel_id));
        }
    });
}

fn handle_open_complete(addr: &str, generation: usize, status: i32) {
    let channel = MAIN_THREAD_STATE.with(|cell| {
        cell.borrow()
            .sessions
            .get(addr)
            .filter(|s| s.generation == generation && !s.opened)
            .map(|s| (s.channel.clone(), s.active.clone()))
    });
    let Some((channel, active)) = channel else {
        return;
    };
    if status != kIOReturnSuccess {
        retry_connection(addr, format!("RFCOMM open completion failed ({})", status));
        return;
    }
    let mtu = unsafe { channel.getMTU() as usize }.clamp(1, u16::MAX as usize);
    let info = MAIN_THREAD_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let session = state.sessions.get_mut(addr)?;
        session.opened = true;
        session.mtu = mtu;
        state.connections.get(addr).map(|c| c.info.clone())
    });
    if let Some(info) = info {
        if let Ok(mut state) = SHARED_BT_STATE.lock() {
            state.connected_device_info.insert(addr.to_owned(), info);
        }
        release_connection_slot(addr);
        notify_connected(addr, active);
        log::info!("RFCOMM connected to {} (MTU {})", addr, mtu);
    }
}

/* ---------- 对外跨平台接口 ---------- */
pub mod core {
    use super::*;

    fn normalized_fallback_channels(channels: &[u8]) -> VecDeque<BluetoothRFCOMMChannelID> {
        let mut out = VecDeque::new();
        for channel in channels.iter().copied().filter(|ch| (1..=30).contains(ch)) {
            if !out.contains(&channel) {
                out.push_back(channel);
            }
        }
        if out.is_empty() {
            out.extend([5, 1]);
        }
        out
    }

    pub fn start_scan_impl() -> Result<()> {
        run_on_main_thread(|mtm| {
            stop_scan_impl()?;
            // Starting inquiry while paging/pairing degrades connection success.
            if MAIN_THREAD_STATE.with(|cell| !cell.borrow().connections.is_empty()) {
                corelib::bail_site!("Cannot start Bluetooth inquiry while connecting");
            }
            let delegate = MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                state
                    .delegate
                    .get_or_insert_with(|| BTDelegate::new(mtm, None))
                    .clone()
            });
            if let Ok(mut state) = SHARED_BT_STATE.lock() {
                state.scanned_devices.clear();
                state.scan_loop_running = true;
            }
            if let Err(error) = start_inquiry(&delegate) {
                if let Ok(mut state) = SHARED_BT_STATE.lock() {
                    state.scan_loop_running = false;
                }
                return Err(error);
            }
            Ok(())
        })
    }

    pub fn stop_scan_impl() -> Result<()> {
        run_on_main_thread(|_| {
            if let Ok(mut state) = SHARED_BT_STATE.lock() {
                state.scan_loop_running = false;
            }
            let current = MAIN_THREAD_STATE.with(|cell| cell.borrow_mut().inquiry.take());
            if let Some(inquiry) = current {
                unsafe { inquiry.stop() };
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

    pub fn connect_impl(addr: &str, fallback_channels: &[u8]) -> Result<bool> {
        let addr = normalize_addr_from_macos(addr);
        let channels = normalized_fallback_channels(fallback_channels);
        run_on_main_thread(move |_| {
            let exists = MAIN_THREAD_STATE.with(|cell| {
                let state = cell.borrow();
                state.connections.contains_key(&addr) || state.sessions.contains_key(&addr)
            });
            let retiring = MAIN_THREAD_STATE.with(|cell| {
                cell.borrow().retiring.values().any(|session| {
                    session
                        ._delegate
                        .ivars()
                        .session
                        .as_ref()
                        .is_some_and(|(old_addr, _)| old_addr == &addr)
                })
            });
            if retiring {
                corelib::bail_site!("Previous RFCOMM session is still closing for {}", addr);
            }
            // A duplicate request must not tear down a live or in-progress session.
            if exists {
                return Ok(true);
            }
            stop_scan_impl()?;
            let api_addr = NSString::from_str(&addr_to_macos_format(&addr));
            let device: Option<Retained<IOBluetoothDevice>> = unsafe {
                msg_send![IOBluetoothDevice::class(), deviceWithAddressString: Some(&*api_addr)]
            };
            let device =
                device.ok_or_else(|| corelib::anyhow_site!("Device not found for {}", addr))?;
            let info = SPPDevice {
                name: unsafe { device.nameOrAddress() }.map(|s| s.to_string()),
                address: addr.clone(),
            };
            let generation = next_id();
            MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                state.connections.insert(
                    addr.clone(),
                    PendingConnection {
                        generation,
                        device,
                        info,
                        channels,
                        last_error: String::new(),
                        sdp_resolved: false,
                    },
                );
                state.connect_queue.push_back(addr);
            });
            Queue::main().exec_async(start_next_connection);
            // Accepted, not yet connected: only openComplete publishes device info.
            Ok(true)
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
            Ok(MAIN_THREAD_STATE.with(|cell| {
                cell.borrow()
                    .sessions
                    .get(&addr)
                    .filter(|s| s.opened && !s.failed)
                    .map(|s| s.mtu)
            }))
        })
    }

    pub fn on_connected_impl(addr: &str, cb: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()> {
        let addr = normalize_addr_from_macos(addr);
        SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?
            .on_connected_callbacks
            .insert(addr, Arc::from(cb));
        Ok(())
    }

    pub fn set_data_listener_impl(
        addr: &str,
        mut cb: Box<dyn FnMut(Result<Vec<u8>, String>) + Send + 'static>,
    ) -> Result<()> {
        let addr = normalize_addr_from_macos(addr);
        let (tx, rx) = mpsc::channel::<ReceivedData>();
        std::thread::Builder::new()
            .name(format!("rfcomm-rx-{}", addr))
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    if event
                        .active
                        .as_ref()
                        .is_some_and(|active| !active.load(Ordering::Acquire))
                    {
                        continue;
                    }
                    let closed = event.data.is_err();
                    cb(event.data);
                    if closed {
                        break;
                    }
                }
            })?;
        SHARED_BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?
            .data_listeners
            .insert(addr, tx);
        Ok(())
    }

    pub fn start_subscription_impl(_addr: &str) -> Result<()> {
        Ok(())
    }

    pub fn send_impl(addr: &str, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if MainThreadMarker::new().is_some() {
            corelib::bail_site!("Synchronous RFCOMM send cannot wait on the main run loop");
        }
        let addr = normalize_addr_from_macos(addr);
        // Copy once on the caller thread, not one allocation per MTU on the UI thread.
        let payload = Arc::new(data.to_vec());
        let (tx, rx) = mpsc::channel();
        let send_addr = addr.clone();
        let (generation, id) = run_on_main_thread(move |_| {
            let id = next_id();
            let generation = MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                let session = state
                    .sessions
                    .get_mut(&send_addr)
                    .filter(|s| s.opened && !s.failed)
                    .ok_or_else(|| {
                        corelib::anyhow_site!("Device not connected, cannot send data")
                    })?;
                if session.pending_send.is_some() {
                    corelib::bail_site!("RFCOMM send already in progress");
                }
                session.pending_send = Some(PendingRfcommSend {
                    id,
                    payload,
                    next_offset: 0,
                    in_flight: 0,
                    completion_tx: tx,
                });
                Ok::<_, anyhow::Error>(session.generation)
            })?;
            pump_pending_rfcomm_send(&send_addr, generation);
            Ok::<_, anyhow::Error>((generation, id))
        })?;
        match rx.recv_timeout(RFCOMM_ASYNC_SEND_TIMEOUT) {
            Ok(result) => result,
            Err(error) => {
                let reason = format!(
                    "RFCOMM async send did not complete within {:?}: {}",
                    RFCOMM_ASYNC_SEND_TIMEOUT, error
                );
                let close_reason = reason.clone();
                run_on_main_thread(move |_| {
                    let current = MAIN_THREAD_STATE.with(|cell| {
                        cell.borrow().sessions.get(&addr).is_some_and(|s| {
                            s.generation == generation
                                && s.pending_send.as_ref().is_some_and(|p| p.id == id)
                        })
                    });
                    if current {
                        // Delivery is now ambiguous: do not reuse the byte stream or
                        // free buffers while a previous write may still own them.
                        fail_connection(&addr, close_reason);
                    }
                });
                Err(corelib::anyhow_site!("{}", reason))
            }
        }
    }

    pub fn disconnect_impl(addr: &str) -> Result<()> {
        let addr = normalize_addr_from_macos(addr);
        run_on_main_thread(move |_| {
            close_session(&addr, "Connection disconnected during RFCOMM send", false);
            audio_guard::stop_for(&addr);
            clear_device_state(&addr, None);
            release_connection_slot(&addr);
            Ok(())
        })
    }

    pub fn disconnect_all_impl() -> Result<()> {
        run_on_main_thread(|_| {
            let addrs = MAIN_THREAD_STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                state.connect_queue.clear();
                state.connecting = None;
                state.connections.clear();
                state.sessions.keys().cloned().collect::<Vec<_>>()
            });
            for addr in addrs {
                close_session(&addr, "Bluetooth shutdown", false);
            }
            let mut state = SHARED_BT_STATE
                .lock()
                .map_err(|_| corelib::anyhow_site!("Failed to acquire Bluetooth state lock"))?;
            state.connected_device_info.clear();
            state.on_connected_callbacks.clear();
            state.data_listeners.clear();
            drop(state);
            audio_guard::stop_all();
            Ok(())
        })
    }
}

pub fn cleanup_bluetooth_resources() {
    let _ = core::stop_scan_impl();
    let _ = core::disconnect_all_impl();
}
