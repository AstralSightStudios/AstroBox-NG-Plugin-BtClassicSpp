use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::Bluetooth::{
    BluetoothAuthenticateDeviceEx, BluetoothFindDeviceClose, BluetoothFindFirstDevice,
    BluetoothFindNextDevice, BluetoothGetDeviceInfo, MITMProtectionNotRequired, AF_BTH,
    BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS, HBLUETOOTH_DEVICE_FIND, SOCKADDR_BTH,
};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, FALSE, HANDLE, TRUE,
    WAIT_OBJECT_0,
};
use windows::Win32::Networking::WinSock::{
    closesocket, connect, recv, send, shutdown, socket, WSACleanup, WSAGetLastError, WSAStartup,
    INVALID_SOCKET, SD_BOTH, SEND_RECV_FLAGS, SOCKET, SOCK_STREAM, WSADATA, WSAEWOULDBLOCK,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use crate::SPPDevice;

const BTHPROTO_RFCOMM: i32 = 3;
const RFCOMM_PORT_ANY: u32 = 0;

const SPP_SERVICE_CLASS_UUID: GUID = GUID::from_u128(0x00001101_0000_1000_8000_00805F9B34FB);
const SPP_UUID_CONNECT_RETRY_COUNT: usize = 3;

type ConnectedCallback = dyn Fn() + Send + Sync + 'static;
type DataListener = dyn FnMut(Result<Vec<u8>, String>) + Send + 'static;
type SharedConnectedCallback = Arc<ConnectedCallback>;
type SharedDataListener = Arc<Mutex<Box<DataListener>>>;

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn is_invalid(&self) -> bool {
        self.0.is_invalid()
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.is_invalid() {
            unsafe {
                CloseHandle(self.0).ok();
            }
        }
    }
}

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

struct DeviceFindHandle(HBLUETOOTH_DEVICE_FIND);

impl DeviceFindHandle {
    fn raw(&self) -> HBLUETOOTH_DEVICE_FIND {
        self.0
    }
}

impl Drop for DeviceFindHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                BluetoothFindDeviceClose(self.0).ok();
            }
        }
    }
}

fn bth_addr_to_string(addr: u64) -> String {
    let bytes = addr.to_be_bytes();
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
    )
}

fn string_to_bth_addr(addr_str: &str) -> Result<u64> {
    let parts: Vec<&str> = addr_str.split(':').collect();
    if parts.len() != 6 {
        corelib::bail_site!("Invalid Bluetooth address format: {}", addr_str);
    }
    let mut bytes = [0u8; 8];
    for i in 0..6 {
        bytes[7 - i] = u8::from_str_radix(parts[5 - i], 16)
            .with_context(|| format!("Invalid hex byte in address: {}", parts[5 - i]))?;
    }
    Ok(u64::from_be_bytes(bytes))
}

fn string_from_utf16_char_array(chars: &[u16]) -> String {
    let len = chars.iter().take_while(|&&c| c != 0).count();
    String::from_utf16_lossy(&chars[..len])
}

fn create_manual_reset_event() -> Result<Arc<OwnedHandle>> {
    let handle = unsafe { CreateEventW(None, TRUE.into(), FALSE.into(), PCWSTR::null())? };
    if handle.is_invalid() {
        corelib::bail_site!("CreateEventW returned an invalid handle");
    }
    Ok(Arc::new(OwnedHandle::new(handle)))
}

struct ConnectedThreadHandles {
    socket: SOCKET,
    read_thread_handle: Option<thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    socket_close_lock: Arc<Mutex<()>>,
    connection_id: u64,
}

struct DisconnectedConnection {
    socket: SOCKET,
    read_thread_handle: Option<thread::JoinHandle<()>>,
    socket_close_lock: Arc<Mutex<()>>,
    connection_id: u64,
}

struct GlobalState {
    wsa_initialized: bool,
    scanned_devices: Vec<SPPDevice>,
    is_scanning: bool,
    scan_stop_event: Option<Arc<OwnedHandle>>,
    scan_thread_handle: Option<thread::JoinHandle<()>>,
    connected_device_info: Option<SPPDevice>,
    connection_handles: Option<ConnectedThreadHandles>,
    next_connection_id: u64,
    on_connected_callback: Option<SharedConnectedCallback>,
    data_listener_callback: Option<SharedDataListener>,
}

impl GlobalState {
    fn new() -> Self {
        GlobalState {
            wsa_initialized: false,
            scanned_devices: Vec::new(),
            is_scanning: false,
            scan_stop_event: None,
            scan_thread_handle: None,
            connected_device_info: None,
            connection_handles: None,
            next_connection_id: 1,
            on_connected_callback: None,
            data_listener_callback: None,
        }
    }
}

static BT_STATE: Lazy<Arc<Mutex<GlobalState>>> =
    Lazy::new(|| Arc::new(Mutex::new(GlobalState::new())));

pub mod core {

    use super::*;

    fn normalized_fallback_channels(fallback_channels: &[u8]) -> Vec<u8> {
        let source: Vec<u8> = if fallback_channels.is_empty() {
            vec![5, 1]
        } else {
            fallback_channels.to_vec()
        };

        let mut out = Vec::new();
        for channel in source {
            if channel != 0 && !out.contains(&channel) {
                out.push(channel);
            }
        }
        if out.is_empty() {
            out.extend([5, 1]);
        }
        out
    }

    fn remember_scanned_device(
        state: &Arc<Mutex<GlobalState>>,
        device_info: &BLUETOOTH_DEVICE_INFO,
        log_prefix: &str,
    ) {
        let name_str = string_from_utf16_char_array(&device_info.szName);
        let address_str = bth_addr_to_string(unsafe { device_info.Address.Anonymous.ullLong });
        debug!(
            "{}: Name: '{}', Address: {}, Paired: {}, Connected: {}",
            log_prefix,
            name_str,
            address_str,
            device_info.fAuthenticated == TRUE,
            device_info.fConnected == TRUE
        );

        let dev = SPPDevice {
            name: Some(name_str),
            address: address_str,
        };
        match state.lock() {
            Ok(mut state_w) => {
                if !state_w
                    .scanned_devices
                    .iter()
                    .any(|d| d.address == dev.address)
                {
                    state_w.scanned_devices.push(dev.clone());
                    info!("Added new device to list: {}", dev.address);
                }
            }
            Err(_) => warn!("Failed to lock BT_STATE to remember scanned device"),
        }
    }

    fn init_winsock_if_needed() -> Result<()> {
        let mut state = BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for winsock init"))?;
        if !state.wsa_initialized {
            let mut wsa_data = WSADATA::default();
            let result = unsafe { WSAStartup(0x0202, &mut wsa_data) };
            if result != 0 {
                corelib::bail_site!("WSAStartup failed with error: {}", result);
            }
            state.wsa_initialized = true;
            info!("WinSock initialized.");
        }
        Ok(())
    }

    fn notify_data_listener(result: Result<Vec<u8>, String>) {
        let callback = match BT_STATE.lock() {
            Ok(state) => state.data_listener_callback.clone(),
            Err(_) => {
                warn!("Failed to lock BT_STATE for data listener callback");
                None
            }
        };

        if let Some(callback) = callback {
            match callback.lock() {
                Ok(mut cb) => cb(result),
                Err(_) => warn!("Data listener callback mutex is poisoned"),
            }
        }
    }

    fn close_disconnected_socket(disconnected: DisconnectedConnection) {
        let DisconnectedConnection {
            socket,
            read_thread_handle,
            socket_close_lock,
            connection_id,
        } = disconnected;

        if let Some(handle) = read_thread_handle {
            if thread::current().id() != handle.thread().id() {
                handle
                    .join()
                    .unwrap_or_else(|e| warn!("Read thread join error: {:?}", e));
            }
        }

        match socket_close_lock.lock() {
            Ok(_guard) => unsafe {
                if closesocket(socket) != 0 {
                    warn!(
                        "closesocket failed for connection {}: {}",
                        connection_id,
                        WSAGetLastError().0
                    );
                }
            },
            Err(_) => warn!(
                "Socket close lock poisoned for connection {}; socket may leak",
                connection_id
            ),
        };
    }

    fn finish_disconnect(disconnected: Option<DisconnectedConnection>) {
        if let Some(disconnected) = disconnected {
            close_disconnected_socket(disconnected);
        }
    }

    pub fn bluetooth_stack_cleanup() {
        let (scan_stop_event_opt, scan_thread_handle_opt, disconnected_opt, need_wsa_cleanup) = {
            let mut state = match BT_STATE.lock() {
                Ok(s) => s,
                Err(_) => {
                    warn!("Failed to lock BT_STATE for cleanup. WSA might not be cleaned up.");
                    return;
                }
            };

            let scan_thread_handle_opt = state.scan_thread_handle.take();
            let scan_stop_event_opt = if state.is_scanning || scan_thread_handle_opt.is_some() {
                state.is_scanning = false;
                state.scan_stop_event.take()
            } else {
                None
            };

            let disconnected_opt = disconnect_internal(&mut state);

            let need_wsa_cleanup = state.wsa_initialized;
            if need_wsa_cleanup {
                state.wsa_initialized = false;
            }

            (
                scan_stop_event_opt,
                scan_thread_handle_opt,
                disconnected_opt,
                need_wsa_cleanup,
            )
        };

        if let Some(stop_event) = scan_stop_event_opt {
            unsafe {
                SetEvent(stop_event.raw()).ok();
            }
        }
        if let Some(handle) = scan_thread_handle_opt {
            handle
                .join()
                .unwrap_or_else(|e| warn!("Scan thread join error on cleanup: {:?}", e));
        }
        finish_disconnect(disconnected_opt);
        if need_wsa_cleanup {
            unsafe { WSACleanup() };
            info!("WinSock cleaned up.");
        }
    }

    pub fn start_scan_impl() -> Result<()> {
        init_winsock_if_needed()?;
        stop_scan_impl()?;
        let state_clone_for_thread = Arc::clone(&BT_STATE);
        let mut state_guard = BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for start_scan"))?;

        if state_guard.is_scanning {
            info!("Scan already in progress.");
            return Ok(());
        }
        state_guard.scanned_devices.clear();

        let stop_event_handle = create_manual_reset_event()?;
        state_guard.scan_stop_event = Some(Arc::clone(&stop_event_handle));
        state_guard.is_scanning = true;

        let stop_event_for_thread = Arc::clone(&stop_event_handle);

        state_guard.scan_thread_handle = Some(thread::spawn(move || {
            let stop_event_handle = stop_event_for_thread.raw();
            const INQUIRY_CYCLE_PAUSE_MS: u32 = 3000;

            'outer_scan_loop: loop {
                let initial_wait_result = unsafe { WaitForSingleObject(stop_event_handle, 0) };
                if initial_wait_result == WAIT_OBJECT_0 {
                    info!("Continuous scan: Stop signal received before new inquiry cycle.");
                    break 'outer_scan_loop;
                }

                let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
                    dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
                    fReturnAuthenticated: TRUE,
                    fReturnRemembered: TRUE,
                    fReturnUnknown: TRUE,
                    fReturnConnected: TRUE,
                    fIssueInquiry: TRUE,
                    cTimeoutMultiplier: 3,
                    hRadio: HANDLE::default(),
                };
                let mut device_info = BLUETOOTH_DEVICE_INFO {
                    dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
                    ..Default::default()
                };

                info!(
                    "Continuous scan: Starting new inquiry cycle (timeout ~{}s)...",
                    params.cTimeoutMultiplier as f32 * 1.28
                );

                let find_handle_result =
                    unsafe { BluetoothFindFirstDevice(&params, &mut device_info) };

                let find_handle: HBLUETOOTH_DEVICE_FIND = match find_handle_result {
                    Ok(h) if !h.is_invalid() => h,
                    Ok(invalid_h) => {
                        error!(
                            "Continuous scan: BluetoothFindFirstDevice returned Ok with an invalid handle: {:?}. Win32 Error: {:?}. Pausing before retry.",
                            invalid_h,
                            unsafe { GetLastError() }
                        );
                        let pause_wait = unsafe {
                            WaitForSingleObject(stop_event_handle, INQUIRY_CYCLE_PAUSE_MS)
                        };
                        if pause_wait == WAIT_OBJECT_0 {
                            break 'outer_scan_loop;
                        }
                        continue 'outer_scan_loop;
                    }
                    Err(e) => {
                        error!(
                            "Continuous scan: BluetoothFindFirstDevice failed. Error: {:?} (Win32: {:?}). Pausing before retry.",
                            e,
                            unsafe { GetLastError() }
                        );
                        let pause_wait = unsafe {
                            WaitForSingleObject(stop_event_handle, INQUIRY_CYCLE_PAUSE_MS)
                        };
                        if pause_wait == WAIT_OBJECT_0 {
                            break 'outer_scan_loop;
                        }
                        continue 'outer_scan_loop;
                    }
                };
                let find_handle = DeviceFindHandle(find_handle);

                remember_scanned_device(
                    &state_clone_for_thread,
                    &device_info,
                    "Continuous scan found",
                );

                'inner_device_loop: loop {
                    let inner_wait_result = unsafe { WaitForSingleObject(stop_event_handle, 0) };
                    if inner_wait_result == WAIT_OBJECT_0 {
                        info!("Continuous scan: Stop signal received during inner device loop.");
                        break 'outer_scan_loop;
                    }

                    device_info = BLUETOOTH_DEVICE_INFO {
                        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
                        ..Default::default()
                    };

                    match unsafe { BluetoothFindNextDevice(find_handle.raw(), &mut device_info) } {
                        Ok(_) => {
                            remember_scanned_device(
                                &state_clone_for_thread,
                                &device_info,
                                "Continuous scan found next",
                            );
                        }
                        Err(e) => {
                            if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() {
                                debug!(
                                    "Continuous scan: BluetoothFindNextDevice: No more items in this cycle."
                                );
                            } else {
                                error!(
                                    "Continuous scan: BluetoothFindNextDevice error: {:?} (Win32: {:?})",
                                    e,
                                    unsafe { GetLastError() }
                                );
                            }
                            break 'inner_device_loop;
                        }
                    }
                }
                info!("Continuous scan: Finished one inquiry cycle.");

                let pause_wait_result =
                    unsafe { WaitForSingleObject(stop_event_handle, INQUIRY_CYCLE_PAUSE_MS) };
                if pause_wait_result == WAIT_OBJECT_0 {
                    info!("Continuous scan: Stop signal received during pause between cycles.");
                    break 'outer_scan_loop;
                }
            }

            info!("Continuous Bluetooth device scan thread finished.");
            if let Ok(mut state_w) = state_clone_for_thread.lock() {
                state_w.is_scanning = false;
            } else {
                warn!("Failed to lock BT_STATE when scan thread finished");
            }
        }));
        Ok(())
    }

    pub fn stop_scan_impl() -> Result<()> {
        let (stop_event_opt, thread_handle_opt) = {
            let mut state = BT_STATE
                .lock()
                .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for stop_scan"))?;

            if !state.is_scanning && state.scan_thread_handle.is_none() {
                return Ok(());
            }

            let stop_evt = state.scan_stop_event.take();
            let th_handle = state.scan_thread_handle.take();
            state.is_scanning = false;
            (stop_evt, th_handle)
        };

        if let Some(stop_event) = stop_event_opt {
            info!("Signaling scan thread to stop...");
            unsafe { SetEvent(stop_event.raw()).context("Failed to set scan stop event")? };
        }

        if let Some(handle) = thread_handle_opt {
            info!("Waiting for scan thread to join...");
            handle
                .join()
                .map_err(|e| corelib::anyhow_site!("Failed to join scan thread: {:?}", e))?;
            info!("Scan thread joined successfully.");
        }

        Ok(())
    }

    pub fn get_scanned_devices_impl() -> Result<Vec<SPPDevice>> {
        let state = BT_STATE.lock().map_err(|_| {
            corelib::anyhow_site!("Failed to lock BT_STATE for get_scanned_devices")
        })?;
        Ok(state.scanned_devices.clone())
    }

    fn attempt_connect(
        device_addr: u64,
        service_guid: Option<GUID>,
        port_channel: Option<u32>,
    ) -> Result<SOCKET> {
        let mut sockaddr = SOCKADDR_BTH {
            addressFamily: AF_BTH,
            btAddr: device_addr,
            serviceClassId: GUID::default(),
            port: 0,
        };

        if let Some(guid) = service_guid {
            sockaddr.serviceClassId = guid;
            sockaddr.port = RFCOMM_PORT_ANY;
        } else if let Some(ch) = port_channel {
            sockaddr.port = ch;
        } else {
            corelib::bail_site!(
                "Either service_guid or port_channel must be specified for connect attempt"
            );
        }

        let sock = unsafe { socket(AF_BTH.into(), SOCK_STREAM.into(), BTHPROTO_RFCOMM.into())? };
        if sock == INVALID_SOCKET {
            corelib::bail_site!("Failed to create socket: {}", unsafe {
                WSAGetLastError().0
            });
        }

        let log_service_class_id = sockaddr.serviceClassId;
        let log_port = sockaddr.port;
        info!(
            "Attempting to connect to addr {} with service_guid {:032X} and port/channel {}",
            bth_addr_to_string(device_addr),
            log_service_class_id.to_u128(),
            log_port
        );

        let connect_result = unsafe {
            connect(
                sock,
                &sockaddr as *const SOCKADDR_BTH as _,
                std::mem::size_of::<SOCKADDR_BTH>() as i32,
            )
        };
        if connect_result != 0 {
            let err_code = unsafe { WSAGetLastError().0 };
            unsafe { closesocket(sock) };
            let bail_service_class_id = sockaddr.serviceClassId;
            let bail_port = sockaddr.port;
            corelib::bail_site!(
                "Connection failed for service {:032X}/port {}: Win32 Error {}",
                bail_service_class_id.to_u128(),
                bail_port,
                err_code
            );
        }
        let success_service_class_id = sockaddr.serviceClassId;
        let success_port = sockaddr.port;
        info!(
            "Successfully connected using service {:032X}/port {}",
            success_service_class_id.to_u128(),
            success_port
        );
        /*
        // 不启用非阻塞模式，因为会造成一些难以处理的问题
        let mut nonblocking: u32 = 1;
        unsafe {
            ioctlsocket(sock, FIONBIO as _, &mut nonblocking);
        }*/
        Ok(sock)
    }

    pub fn connect_impl(addr_str: &str) -> Result<bool> {
        connect_impl_with_fallback_channels(addr_str, &[5, 1])
    }

    pub fn connect_impl_with_fallback_channels(
        addr_str: &str,
        fallback_channels: &[u8],
    ) -> Result<bool> {
        init_winsock_if_needed()?;
        stop_scan_impl()?;
        let fallback_channels = normalized_fallback_channels(fallback_channels);

        let old_connection_opt = {
            let mut state = BT_STATE
                .lock()
                .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for connect"))?;

            if let Some(ref dev_info) = state.connected_device_info {
                if dev_info.address == addr_str {
                    info!("Already connected to this device.");
                    return Ok(true);
                }
                warn!(
                    "Connecting to a new device, disconnecting the old one: {}",
                    dev_info.address
                );
            }
            disconnect_internal(&mut state)
        };

        finish_disconnect(old_connection_opt);

        let target_bth_addr = string_to_bth_addr(addr_str)?;
        let mut device_info_struct = BLUETOOTH_DEVICE_INFO {
            dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
            ..Default::default()
        };
        device_info_struct.Address.Anonymous.ullLong = target_bth_addr;

        let get_device_info_err = unsafe { BluetoothGetDeviceInfo(None, &mut device_info_struct) };
        if get_device_info_err != ERROR_SUCCESS.0 {
            warn!(
                "BluetoothGetDeviceInfo for {} failed initially or device not remembered. Error code: {}. This is expected for unbonded devices.",
                addr_str, get_device_info_err
            );
            device_info_struct.fAuthenticated = FALSE;
        } else {
            info!(
                "Device {} name: '{}', authenticated: {}",
                addr_str,
                string_from_utf16_char_array(&device_info_struct.szName),
                device_info_struct.fAuthenticated == TRUE
            );
        }

        if device_info_struct.fAuthenticated != TRUE {
            info!(
                "Device {} is not authenticated (paired). Attempting to pair now...",
                addr_str
            );
            let auth_result = unsafe {
                BluetoothAuthenticateDeviceEx(
                    None,
                    None,
                    &mut device_info_struct,
                    None,
                    MITMProtectionNotRequired,
                )
            };

            if auth_result == ERROR_SUCCESS.0 {
                info!(
                    "BluetoothAuthenticateDeviceEx call returned ERROR_SUCCESS for {}. Device should now be authenticated (paired). fAuthenticated flag is: {}",
                    addr_str,
                    device_info_struct.fAuthenticated == TRUE
                );
                if device_info_struct.fAuthenticated != TRUE {
                    warn!(
                        "Device {} pairing initiated by BluetoothAuthenticateDeviceEx, but fAuthenticated is still false. Pairing might be in progress or require further action.",
                        addr_str
                    );
                }
                thread::sleep(Duration::from_millis(800));
                let refresh_result =
                    unsafe { BluetoothGetDeviceInfo(None, &mut device_info_struct) };
                if refresh_result != ERROR_SUCCESS.0 {
                    warn!(
                        "BluetoothGetDeviceInfo refresh after pairing for {} failed. Error code: {}.",
                        addr_str, refresh_result
                    );
                }
            } else {
                warn!(
                    "BluetoothAuthenticateDeviceEx call failed for {}. Error code: {}. Pairing may require user interaction or failed. Connection attempt will proceed.",
                    addr_str, auth_result
                );
            }
        }

        let mut sock_opt = None;
        let mut last_error: Option<anyhow::Error> = None;
        for attempt in 1..=SPP_UUID_CONNECT_RETRY_COUNT {
            match attempt_connect(target_bth_addr, Some(SPP_SERVICE_CLASS_UUID), None) {
                Ok(sock) => {
                    sock_opt = Some(sock);
                    break;
                }
                Err(e) => {
                    warn!(
                        "SPP Service UUID attempt {}/{} failed for {}: {:?}",
                        attempt, SPP_UUID_CONNECT_RETRY_COUNT, addr_str, e
                    );
                    last_error = Some(e);
                    if attempt < SPP_UUID_CONNECT_RETRY_COUNT {
                        thread::sleep(Duration::from_millis(250 * attempt as u64));
                    }
                }
            }
        }

        if sock_opt.is_none() {
            info!(
                "Windows RFCOMM fallback channel attempt order for {}: {:?}",
                addr_str, fallback_channels
            );
            for channel in fallback_channels {
                warn!(
                    "Trying RFCOMM channel {} for {} after SPP UUID failure...",
                    channel, addr_str
                );
                match attempt_connect(target_bth_addr, None, Some(channel as u32)) {
                    Ok(sock) => {
                        sock_opt = Some(sock);
                        break;
                    }
                    Err(e) => {
                        warn!(
                            "RFCOMM Channel {} failed for {}: {:?}",
                            channel, addr_str, e
                        );
                        last_error = Some(e);
                    }
                }
            }
        }

        let sock = sock_opt.ok_or_else(|| {
            corelib::anyhow_site!(
                "SPP Service UUID and fallback RFCOMM channels failed for {}: {:?}",
                addr_str,
                last_error
            )
        })?;
        info!("SPP Connection successful to {}", addr_str);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let socket_close_lock = Arc::new(Mutex::new(()));

        let (connected_callback, displaced_connection) = match BT_STATE.lock() {
            Ok(mut state) => {
                let displaced_connection = if state.connection_handles.is_some() {
                    warn!("Connection state changed while connecting; replacing old connection.");
                    disconnect_internal(&mut state)
                } else {
                    None
                };

                let connection_id = state.next_connection_id;
                state.next_connection_id = state.next_connection_id.wrapping_add(1);
                if state.next_connection_id == 0 {
                    state.next_connection_id = 1;
                }

                state.connection_handles = Some(ConnectedThreadHandles {
                    socket: sock,
                    read_thread_handle: None,
                    stop_flag: Arc::clone(&stop_flag),
                    socket_close_lock: Arc::clone(&socket_close_lock),
                    connection_id,
                });
                state.connected_device_info = Some(SPPDevice {
                    name: Some(string_from_utf16_char_array(&device_info_struct.szName)),
                    address: addr_str.to_string(),
                });

                (state.on_connected_callback.clone(), displaced_connection)
            }
            Err(_) => {
                unsafe {
                    closesocket(sock);
                }
                return Err(corelib::anyhow_site!(
                    "Failed to lock BT_STATE post connection"
                ));
            }
        };

        finish_disconnect(displaced_connection);

        if let Some(callback) = connected_callback {
            callback();
        }

        Ok(true)
    }

    pub fn get_connected_device_info_impl() -> Result<Option<SPPDevice>> {
        let state = BT_STATE.lock().map_err(|_| {
            corelib::anyhow_site!("Failed to lock BT_STATE for get_connected_device_info")
        })?;
        Ok(state.connected_device_info.clone())
    }

    pub fn get_max_send_len_impl() -> Result<Option<usize>> {
        Ok(None)
    }

    pub fn on_connected_impl(cb: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()> {
        let cb: SharedConnectedCallback = Arc::from(cb);
        let should_call_now = {
            let mut state = BT_STATE
                .lock()
                .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for on_connected"))?;
            let should_call_now = state.connected_device_info.is_some();
            state.on_connected_callback = Some(Arc::clone(&cb));
            should_call_now
        };

        if should_call_now {
            cb();
        }

        Ok(())
    }

    pub fn set_data_listener_impl(
        cb: Box<dyn FnMut(Result<Vec<u8>, String>) + Send + 'static>,
    ) -> Result<()> {
        let mut state = BT_STATE
            .lock()
            .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for set_data_listener"))?;
        state.data_listener_callback = Some(Arc::new(Mutex::new(cb)));
        Ok(())
    }

    pub fn start_subscription_impl() -> Result<()> {
        let (sock_copy, stop_flag, connection_id) = {
            let state_guard = BT_STATE.lock().map_err(|_| {
                corelib::anyhow_site!(
                    "Failed to lock BT_STATE for start_subscription (initial check)"
                )
            })?;

            if let Some(ref handles) = state_guard.connection_handles {
                if handles.read_thread_handle.is_none() {
                    (
                        handles.socket,
                        Arc::clone(&handles.stop_flag),
                        handles.connection_id,
                    )
                } else {
                    warn!("Subscription already active or read thread handle exists.");
                    return Ok(());
                }
            } else {
                corelib::bail_site!("Not connected. Cannot start subscription.");
            }
        };

        let state_clone_for_thread = Arc::clone(&BT_STATE);
        let stop_flag_for_thread = Arc::clone(&stop_flag);
        let read_thread_handle = thread::spawn(move || {
            info!(
                "Read thread started for connection {} socket {:?}",
                connection_id, sock_copy
            );
            let mut buffer = [0u8; 1024];

            loop {
                if stop_flag_for_thread.load(Ordering::Acquire) {
                    info!(
                        "Read thread received stop signal before recv for connection {}.",
                        connection_id
                    );
                    break;
                }

                let bytes_received = unsafe { recv(sock_copy, &mut buffer, SEND_RECV_FLAGS(0)) };

                if bytes_received > 0 {
                    let data = buffer[..bytes_received as usize].to_vec();
                    notify_data_listener(Ok(data));
                } else if bytes_received == 0 {
                    if stop_flag_for_thread.load(Ordering::Acquire) {
                        info!(
                            "Read thread observed local shutdown for connection {}.",
                            connection_id
                        );
                    } else {
                        info!("Connection closed by peer (socket {:?}).", sock_copy);
                        notify_data_listener(Err("Connection closed by peer".to_string()));
                    }
                    break;
                } else {
                    let error_code = unsafe { WSAGetLastError() };
                    if stop_flag_for_thread.load(Ordering::Acquire) {
                        info!(
                            "recv stopped after local shutdown for connection {}: {}",
                            connection_id, error_code.0
                        );
                        break;
                    }
                    if error_code.0 == WSAEWOULDBLOCK.0 {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    error!(
                        "recv failed with error: {} (socket {:?})",
                        error_code.0, sock_copy
                    );
                    notify_data_listener(Err(format!("Socket error: {}", error_code.0)));
                    break;
                }
            }

            info!(
                "Read thread stopped for connection {} socket {:?}",
                connection_id, sock_copy
            );

            let disconnected = match state_clone_for_thread.lock() {
                Ok(mut st_lock) => {
                    let owns_current_connection =
                        st_lock.connection_handles.as_ref().is_some_and(|handles| {
                            handles.connection_id == connection_id && handles.socket == sock_copy
                        });
                    if owns_current_connection {
                        disconnect_internal(&mut st_lock)
                    } else {
                        None
                    }
                }
                Err(_) => {
                    warn!("Failed to lock BT_STATE when read thread stopped");
                    None
                }
            };
            finish_disconnect(disconnected);
        });

        let mut state_guard = BT_STATE.lock().map_err(|_| {
            corelib::anyhow_site!("Failed to lock BT_STATE for start_subscription (update handle)")
        })?;

        match state_guard.connection_handles {
            Some(ref mut handles)
                if handles.socket == sock_copy
                    && handles.connection_id == connection_id
                    && handles.read_thread_handle.is_none() =>
            {
                handles.read_thread_handle = Some(read_thread_handle);
                info!("Subscription started successfully.");
            }
            _ => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    if shutdown(sock_copy, SD_BOTH) != 0 {
                        warn!(
                            "socket shutdown failed after subscription race: {}",
                            WSAGetLastError().0
                        );
                    }
                }
                drop(state_guard);
                read_thread_handle
                    .join()
                    .unwrap_or_else(|e| warn!("Read thread join error after race: {:?}", e));
                warn!(
                    "Could not start subscription, state might have changed or connection is different."
                );
            }
        }
        Ok(())
    }

    pub fn send_impl(data: &[u8]) -> Result<()> {
        let (socket, stop_flag, socket_close_lock) = {
            let state = BT_STATE
                .lock()
                .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for send"))?;
            if let Some(ref handles) = state.connection_handles {
                (
                    handles.socket,
                    Arc::clone(&handles.stop_flag),
                    Arc::clone(&handles.socket_close_lock),
                )
            } else {
                corelib::bail_site!("Not connected. Cannot send data.");
            }
        };

        if stop_flag.load(Ordering::Acquire) {
            corelib::bail_site!("Connection is closing. Cannot send data.");
        }

        let _close_guard = socket_close_lock
            .lock()
            .map_err(|_| corelib::anyhow_site!("Socket close lock poisoned during send"))?;

        if stop_flag.load(Ordering::Acquire) {
            corelib::bail_site!("Connection is closing. Cannot send data.");
        }

        let mut total_sent = 0;
        while total_sent < data.len() {
            let sent = unsafe { send(socket, &data[total_sent..], SEND_RECV_FLAGS(0)) };
            if sent > 0 {
                total_sent += sent as usize;
            } else {
                corelib::bail_site!("send failed with error: {}", unsafe { WSAGetLastError().0 });
            }
        }
        Ok(())
    }

    fn disconnect_internal(state: &mut GlobalState) -> Option<DisconnectedConnection> {
        let disconnected = if let Some(handles) = state.connection_handles.take() {
            info!(
                "Disconnecting connection {} socket {:?}",
                handles.connection_id, handles.socket
            );
            handles.stop_flag.store(true, Ordering::Release);
            unsafe {
                if shutdown(handles.socket, SD_BOTH) != 0 {
                    warn!("socket shutdown failed: {}", WSAGetLastError().0);
                }
            }
            Some(DisconnectedConnection {
                socket: handles.socket,
                read_thread_handle: handles.read_thread_handle,
                socket_close_lock: handles.socket_close_lock,
                connection_id: handles.connection_id,
            })
        } else {
            None
        };
        state.connected_device_info = None;
        if disconnected.is_some() {
            info!("Disconnected.");
        }
        disconnected
    }

    pub fn disconnect_impl() -> Result<()> {
        let disconnected = {
            let mut state = BT_STATE
                .lock()
                .map_err(|_| corelib::anyhow_site!("Failed to lock BT_STATE for disconnect"))?;
            disconnect_internal(&mut state)
        };

        finish_disconnect(disconnected);
        Ok(())
    }
}

pub fn cleanup_bluetooth_resources() {
    core::bluetooth_stack_cleanup();
}
