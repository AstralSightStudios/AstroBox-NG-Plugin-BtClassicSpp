use serde::de::DeserializeOwned;
use tauri::{plugin::PluginApi, AppHandle, Runtime};

#[cfg(target_os = "windows")]
#[path = "./win/implementation.rs"]
pub mod imp;

#[cfg(target_os = "macos")]
#[path = "./macos/implementation.rs"]
pub mod imp;

#[cfg(target_os = "linux")]
#[path = "./linux/implementation.rs"]
pub mod imp;

use imp::core;

use crate::models::*;

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<BtclassicSpp<R>> {
    Ok(BtclassicSpp(app.clone()))
}

/// Access to the btclassic-spp APIs.
pub struct BtclassicSpp<R: Runtime>(AppHandle<R>);

impl<R: Runtime> BtclassicSpp<R> {
    pub fn start_scan(&self) -> anyhow::Result<()> {
        core::start_scan_impl()
    }

    pub fn stop_scan(&self) -> anyhow::Result<()> {
        core::stop_scan_impl()
    }

    pub fn get_scanned_devices(&self) -> anyhow::Result<GetScannedDevicesResult> {
        core::get_scanned_devices_impl().map(|devices| GetScannedDevicesResult { ret: devices })
    }

    pub fn connect(
        &self,
        addr: &str,
        _remove_bond: bool,
        fallback_channels: &[u8],
    ) -> anyhow::Result<ConnectResult> {
        core::connect_impl(addr, fallback_channels).map(|success| ConnectResult { ret: success })
    }

    pub fn get_connected_device_info(&self, addr: &str) -> anyhow::Result<SPPDevice> {
        core::get_connected_device_info_impl(addr)?
            .ok_or_else(|| corelib::anyhow_site!("No device connected for {addr}"))
    }

    pub fn get_max_send_len(&self, addr: &str) -> anyhow::Result<Option<usize>> {
        core::get_max_send_len_impl(addr)
    }

    pub fn on_connected<F>(&self, addr: &str, cb: F) -> anyhow::Result<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        core::on_connected_impl(addr, Box::new(cb))
    }

    pub fn set_data_listener(
        &self,
        addr: &str,
        cb: impl FnMut(Result<Vec<u8>, String>) + Send + 'static,
    ) -> anyhow::Result<()> {
        core::set_data_listener_impl(addr, Box::new(cb))
    }

    pub fn start_subscription(&self, addr: &str) -> anyhow::Result<()> {
        core::start_subscription_impl(addr)
    }

    pub fn send(&self, addr: &str, data: &[u8]) -> anyhow::Result<()> {
        match core::send_impl(addr, data) {
            Ok(()) => Ok(()),
            Err(err) => {
                log::error!("send msg error for {}: {}", addr, err);
                Err(corelib::anyhow_site!("{}", err))
            }
        }
    }

    pub fn disconnect(&self, addr: &str) -> anyhow::Result<()> {
        core::disconnect_impl(addr)
    }

    /// Shut down every transport session during application teardown.
    pub fn disconnect_all(&self) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            core::disconnect_all_impl()?;
            core::stop_scan_impl()?;
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        imp::cleanup_bluetooth_resources();
        Ok(())
    }
}
