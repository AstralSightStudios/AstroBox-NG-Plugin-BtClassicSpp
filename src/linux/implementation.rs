// plugins/btclassic-spp/src/linux/implementation.rs

use crate::models::SPPDevice;
use anyhow::Result;
use bluer::rfcomm::{Profile, ReqError, Role, SocketAddr, Stream};
use bluer::{Adapter, AdapterEvent, Address, DiscoveryFilter, DiscoveryTransport, Session, Uuid};
use futures_util::stream::StreamExt;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

// 创建一个全局、持久化的 Tokio 运行时，专门用于蓝牙操作
static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
});

const SPP_SERVICE_UUID: &str = "00001101-0000-1000-8000-00805f9b34fb";

struct DeviceConnection {
    socket_stream: Arc<Mutex<Stream>>,
    read_stop: Option<Arc<AtomicBool>>,
    read_thread: Option<JoinHandle<()>>,
    connected_device_info: SPPDevice,
    on_connected_callback: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    data_listener_callback: Option<Arc<Mutex<Box<dyn FnMut(Result<Vec<u8>, String>) + Send>>>>,
}

struct GlobalState {
    adapter: Option<Adapter>,
    scanned_devices: Vec<SPPDevice>,
    scan_stop: Option<Arc<AtomicBool>>,
    scan_thread: Option<JoinHandle<()>>,
    connections: HashMap<String, DeviceConnection>,
    // Callbacks may be registered before the corresponding connection exists.
    on_connected_callbacks: HashMap<String, Arc<dyn Fn() + Send + Sync + 'static>>,
    data_listener_callbacks:
        HashMap<String, Arc<Mutex<Box<dyn FnMut(Result<Vec<u8>, String>) + Send>>>>,
}

impl GlobalState {
    fn new() -> Self {
        tokio::task::block_in_place(|| {
            let adapter = RUNTIME.block_on(async {
                let session = match Session::new().await {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("Failed to create bluer session: {}", e);
                        return None;
                    }
                };
                let adapter = match session.default_adapter().await {
                    Ok(a) => a,
                    Err(e) => {
                        log::error!("Failed to get default adapter: {}", e);
                        return None;
                    }
                };
                if let Err(e) = adapter
                    .set_discovery_filter(DiscoveryFilter {
                        transport: DiscoveryTransport::BrEdr,
                        ..Default::default()
                    })
                    .await
                {
                    log::error!("Failed to set discovery filter: {}", e);
                }
                Some(adapter)
            });

            Self {
                adapter,
                scanned_devices: Vec::new(),
                scan_stop: None,
                scan_thread: None,
                connections: HashMap::new(),
                on_connected_callbacks: HashMap::new(),
                data_listener_callbacks: HashMap::new(),
            }
        })
    }
}

static STATE: Lazy<Arc<std::sync::Mutex<GlobalState>>> =
    Lazy::new(|| Arc::new(std::sync::Mutex::new(GlobalState::new())));

pub fn init_bluetooth_stack() {
    log::info!("Initializing Linux Bluetooth stack...");
    Lazy::force(&STATE);
    log::info!("Linux Bluetooth stack initialized.");
}

async fn connect_spp_uuid(addr: Address) -> Result<Stream> {
    let uuid = SPP_SERVICE_UUID
        .parse::<Uuid>()
        .map_err(|err| corelib::anyhow_site!("Invalid SPP UUID: {}", err))?;
    let session = Session::new()
        .await
        .map_err(|err| corelib::anyhow_site!("Failed to create bluer session: {}", err))?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|err| corelib::anyhow_site!("Failed to get default adapter: {}", err))?;
    let device = adapter
        .device(addr)
        .map_err(|err| corelib::anyhow_site!("Failed to open device {}: {}", addr, err))?;
    let mut profile_handle = session
        .register_profile(Profile {
            uuid,
            name: Some("AstroBox SPP Client".to_string()),
            role: Some(Role::Client),
            require_authentication: Some(false),
            require_authorization: Some(false),
            ..Default::default()
        })
        .await
        .map_err(|err| corelib::anyhow_site!("Failed to register SPP profile: {}", err))?;

    let request_future = async move {
        while let Some(request) = profile_handle.next().await {
            if request.device() == addr {
                return request.accept().map_err(|err| {
                    corelib::anyhow_site!("Failed to accept SPP profile fd: {}", err)
                });
            }
            request.reject(ReqError::Rejected);
        }
        Err(corelib::anyhow_site!(
            "SPP profile closed before receiving an RFCOMM fd"
        ))
    };

    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            connect_result = device.connect_profile(&uuid) => {
                connect_result
                    .map_err(|err| corelib::anyhow_site!("BlueZ ConnectProfile failed: {}", err))?;
                Err(corelib::anyhow_site!(
                    "BlueZ ConnectProfile completed without providing an RFCOMM fd"
                ))
            }
            stream_result = request_future => stream_result,
        }
    })
    .await
    .map_err(|_| corelib::anyhow_site!("Timeout connecting through SPP Service UUID"))?
}

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

    pub fn start_scan_impl() -> Result<()> {
        stop_scan_impl()?;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = stop_flag.clone();
        let state_clone = STATE.clone();

        let handle = RUNTIME.spawn(async move {
            let adapter = match state_clone.lock() {
                Ok(guard) => guard.adapter.clone(),
                Err(_) => {
                    log::error!("Failed to lock state for scanning");
                    return;
                }
            };

            let adapter = match adapter {
                Some(a) => a,
                None => {
                    log::error!("Bluetooth adapter not available for scanning.");
                    return;
                }
            };

            if let Err(e) = adapter.set_powered(true).await {
                log::error!("Failed to power on adapter: {}", e);
                return;
            }

            let mut scan_stream = match adapter.discover_devices().await {
                Ok(s) => Box::pin(s),
                Err(e) => {
                    log::error!("Failed to start device discovery: {}", e);
                    return;
                }
            };

            log::info!("Linux Bluetooth scan started.");

            while !stop_flag_thread.load(Ordering::SeqCst) {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    event_opt = scan_stream.next() => {
                        if let Some(event) = event_opt {
                            match event {
                                AdapterEvent::DeviceAdded(addr) => {
                                    if let Ok(device) = adapter.device(addr) {
                                        let name_res = device.name().await;
                                        let info = SPPDevice {
                                            name: name_res.unwrap_or(None),
                                            address: addr.to_string(),
                                        };
                                        let mut st = state_clone.lock().unwrap();
                                        if !st.scanned_devices.iter().any(|d| d.address == info.address) {
                                            log::info!("Found device: {} ({})", info.address, info.name.as_deref().unwrap_or("N/A"));
                                            st.scanned_devices.push(info);
                                        }
                                    }
                                }
                                AdapterEvent::DeviceRemoved(addr) => {
                                     let mut st = state_clone.lock().unwrap();
                                     st.scanned_devices.retain(|d| d.address != addr.to_string());
                                }
                                _ => {}
                            }
                        } else {
                            break;
                        }
                    }
                }
            }

            log::info!("Linux Bluetooth scan stopped.");
            let mut st = state_clone.lock().unwrap();
            st.scan_thread = None;
            st.scan_stop = None;
        });

        let mut st = STATE.lock().unwrap();
        st.scanned_devices.clear();
        st.scan_stop = Some(stop_flag);
        st.scan_thread = Some(handle);
        Ok(())
    }

    pub fn stop_scan_impl() -> Result<()> {
        let (stop_opt, thread_opt) = {
            let mut st = STATE.lock().unwrap();
            (st.scan_stop.take(), st.scan_thread.take())
        };
        if let Some(flag) = stop_opt {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(h) = thread_opt {
            h.abort();
        }
        Ok(())
    }

    pub fn get_scanned_devices_impl() -> Result<Vec<SPPDevice>> {
        let st = STATE.lock().unwrap();
        Ok(st.scanned_devices.clone())
    }

    fn normalize_addr(addr_str: &str) -> Result<(String, Address)> {
        let addr: Address = addr_str
            .parse()
            .map_err(|e| corelib::anyhow_site!("Invalid address format: {}", e))?;
        Ok((addr.to_string(), addr))
    }

    fn stop_connection(connection: DeviceConnection) {
        if let Some(flag) = connection.read_stop {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(thread) = connection.read_thread {
            thread.abort();
        }
        // Dropping the stream closes the RFCOMM connection.
        drop(connection.socket_stream);
    }

    pub fn connect_impl(addr_str: &str, fallback_channels: &[u8]) -> Result<bool> {
        stop_scan_impl()?;
        let (addr_key, addr) = normalize_addr(addr_str)?;
        // Reconnecting one address must not affect any other address. Keep
        // callbacks registered for this upcoming connection; the public
        // disconnect_impl intentionally clears them.
        let old_connection = {
            let mut st = STATE.lock().unwrap();
            st.connections.remove(&addr_key)
        };
        if let Some(old_connection) = old_connection {
            stop_connection(old_connection);
        }
        let fallback_channels = normalized_fallback_channels(fallback_channels);

        tokio::task::block_in_place(|| {
            RUNTIME.block_on(async move {
                let mut stream: Option<Stream> = None;
                let mut last_error: Option<anyhow::Error> = None;

                log::info!(
                    "Attempting to connect to {} via SPP Service UUID {}",
                    addr,
                    SPP_SERVICE_UUID
                );
                match connect_spp_uuid(addr).await {
                    Ok(s) => {
                        log::info!("Successfully connected through SPP Service UUID");
                        stream = Some(s);
                    }
                    Err(e) => {
                        log::warn!("Failed to connect through SPP Service UUID: {}", e);
                        last_error = Some(e);
                    }
                }

                if stream.is_none() {
                    log::info!(
                        "Linux RFCOMM fallback channel attempt order for {}: {:?}",
                        addr,
                        fallback_channels
                    );
                    for &channel in &fallback_channels {
                        let sock_addr = SocketAddr::new(addr, channel);
                        log::info!("Attempting to connect to {} on channel {}", addr, channel);
                        match tokio::time::timeout(
                            Duration::from_secs(10),
                            Stream::connect(sock_addr),
                        )
                        .await
                        {
                            Ok(Ok(s)) => {
                                log::info!("Successfully connected on channel {}", channel);
                                stream = Some(s);
                                break;
                            }
                            Ok(Err(e)) => {
                                log::warn!("Failed to connect on channel {}: {}", channel, e);
                                last_error = Some(e.into());
                            }
                            Err(_) => {
                                log::warn!("Timeout connecting on channel {}", channel);
                                last_error = Some(corelib::anyhow_site!(
                                    "Timeout connecting on channel {}",
                                    channel
                                ));
                            }
                        }
                    }
                }

                let connected_stream = stream.ok_or_else(|| {
                    corelib::anyhow_site!(
                        "Failed to connect through SPP UUID or fallback channels: {:?}",
                        last_error
                    )
                })?;
                let socket_arc = Arc::new(Mutex::new(connected_stream));

                // Fetch the name without holding the synchronous state lock over an await.
                let adapter = STATE.lock().unwrap().adapter.clone();
                let name = if let Some(adapter) = adapter {
                    match adapter.device(addr) {
                        Ok(device) => device.name().await.unwrap_or(None),
                        Err(_) => None,
                    }
                } else {
                    None
                };

                let callback = {
                    let mut st = STATE.lock().unwrap();
                    let connection = DeviceConnection {
                        socket_stream: socket_arc,
                        read_stop: None,
                        read_thread: None,
                        connected_device_info: SPPDevice {
                            name,
                            address: addr_key.clone(),
                        },
                        on_connected_callback: st.on_connected_callbacks.remove(&addr_key),
                        data_listener_callback: st.data_listener_callbacks.remove(&addr_key),
                    };
                    let callback = connection.on_connected_callback.clone();
                    st.connections.insert(addr_key, connection);
                    callback
                };
                if let Some(callback) = callback {
                    callback();
                }
                Ok(true)
            })
        })
    }

    pub fn get_connected_device_info_impl(addr: &str) -> Result<Option<SPPDevice>> {
        let (addr_key, _) = normalize_addr(addr)?;
        let st = STATE.lock().unwrap();
        Ok(st
            .connections
            .get(&addr_key)
            .map(|connection| connection.connected_device_info.clone()))
    }

    pub fn get_max_send_len_impl(_addr: &str) -> Result<Option<usize>> {
        // Linux RFCOMM does not expose a useful fixed application payload limit.
        Ok(None)
    }

    pub fn on_connected_impl(addr: &str, cb: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()> {
        let (addr_key, _) = normalize_addr(addr)?;
        let callback: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(cb);
        let should_call = {
            let mut st = STATE.lock().unwrap();
            if let Some(connection) = st.connections.get_mut(&addr_key) {
                connection.on_connected_callback = Some(callback.clone());
                true
            } else {
                st.on_connected_callbacks.insert(addr_key, callback.clone());
                false
            }
        };
        if should_call {
            callback();
        }
        Ok(())
    }

    pub fn set_data_listener_impl(
        addr: &str,
        cb: Box<dyn FnMut(Result<Vec<u8>, String>) + Send + 'static>,
    ) -> Result<()> {
        let (addr_key, _) = normalize_addr(addr)?;
        let callback = Arc::new(Mutex::new(cb));
        let mut st = STATE.lock().unwrap();
        if let Some(connection) = st.connections.get_mut(&addr_key) {
            connection.data_listener_callback = Some(callback);
        } else {
            st.data_listener_callbacks.insert(addr_key, callback);
        }
        Ok(())
    }

    pub fn start_subscription_impl(addr: &str) -> Result<()> {
        let (addr_key, _) = normalize_addr(addr)?;
        let (socket_arc, cb_arc, stop_flag) = {
            let mut st = STATE.lock().unwrap();
            let connection = st
                .connections
                .get_mut(&addr_key)
                .ok_or_else(|| corelib::anyhow_site!("Not connected"))?;
            if connection.read_thread.is_some() {
                return Ok(());
            }
            let cb = connection
                .data_listener_callback
                .clone()
                .ok_or_else(|| corelib::anyhow_site!("Data listener not set"))?;
            let flag = Arc::new(AtomicBool::new(false));
            connection.read_stop = Some(flag.clone());
            (connection.socket_stream.clone(), cb, flag)
        };

        let socket_for_task = socket_arc.clone();
        let state_clone = STATE.clone();
        let addr_for_task = addr_key.clone();
        let handle = RUNTIME.spawn(async move {
            let mut buf = [0u8; 1024];
            while !stop_flag.load(Ordering::SeqCst) {
                let mut sock_lock = socket_arc.lock().await;
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
                    read_res = sock_lock.read(&mut buf) => {
                        match read_res {
                            Ok(0) => {
                                log::info!("Connection {} closed by peer.", addr_for_task);
                                let mut f = cb_arc.lock().await;
                                f(Err("Connection closed".into()));
                                break;
                            }
                            Ok(n) => {
                                let data = buf[..n].to_vec();
                                let mut f = cb_arc.lock().await;
                                f(Ok(data));
                            }
                            Err(e) => {
                                log::error!("Socket read error for {}: {}", addr_for_task, e);
                                let mut f = cb_arc.lock().await;
                                f(Err(e.to_string()));
                                break;
                            }
                        }
                    }
                }
            }

            // Do not let a completed reader prevent a later subscription.  The
            // pointer check prevents an old reader from touching a replacement
            // connection for the same address.
            let mut st = state_clone.lock().unwrap();
            if let Some(connection) = st.connections.get_mut(&addr_for_task) {
                if Arc::ptr_eq(&connection.socket_stream, &socket_for_task) {
                    connection.read_thread = None;
                    connection.read_stop = None;
                }
            }
        });

        let mut st = STATE.lock().unwrap();
        if let Some(connection) = st.connections.get_mut(&addr_key) {
            connection.read_thread = Some(handle);
            Ok(())
        } else {
            handle.abort();
            Err(corelib::anyhow_site!("Connection was closed"))
        }
    }

    pub fn send_impl(addr: &str, data: &[u8]) -> Result<()> {
        let (addr_key, _) = normalize_addr(addr)?;
        let socket_arc = {
            let st = STATE.lock().unwrap();
            st.connections
                .get(&addr_key)
                .map(|connection| connection.socket_stream.clone())
                .ok_or_else(|| corelib::anyhow_site!("Not connected"))?
        };
        let data_clone = data.to_vec();

        tokio::task::block_in_place(|| {
            RUNTIME.block_on(async {
                let mut sock = socket_arc.lock().await;
                sock.write_all(&data_clone).await?;
                sock.flush().await?;
                Ok(())
            })
        })
    }

    pub fn disconnect_impl(addr: &str) -> Result<()> {
        let (addr_key, _) = normalize_addr(addr)?;
        let connection = {
            let mut st = STATE.lock().unwrap();
            st.on_connected_callbacks.remove(&addr_key);
            st.data_listener_callbacks.remove(&addr_key);
            st.connections.remove(&addr_key)
        };
        if let Some(connection) = connection {
            stop_connection(connection);
        }
        Ok(())
    }

    pub fn disconnect_all_impl() -> Result<()> {
        let connections = {
            let mut st = STATE.lock().unwrap();
            st.on_connected_callbacks.clear();
            st.data_listener_callbacks.clear();
            st.connections
                .drain()
                .map(|(_, connection)| connection)
                .collect::<Vec<_>>()
        };
        for connection in connections {
            stop_connection(connection);
        }
        Ok(())
    }
}
