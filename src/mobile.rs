use base64::{engine::general_purpose, Engine};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::*;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_btclassic_spp);

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<BtclassicSpp<R>> {
    #[cfg(target_os = "android")]
    let handle = api.register_android_plugin(
        "com.astralsight.astrobox.plugin.btclassic_spp",
        "BtClassicSPPPlugin",
    )?;
    // ios插件并没有实际实现，只是为了通过编译
    #[cfg(target_os = "ios")]
    let handle = api.register_ios_plugin(init_plugin_btclassic_spp)?;
    Ok(BtclassicSpp(handle))
}

/// 访问 btclassic-spp API
pub struct BtclassicSpp<R: Runtime>(PluginHandle<R>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressArg<'a> {
    addr: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressChannelArg<'a> {
    addr: &'a str,
    channel: Channel<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressSendPayload {
    addr: String,
    b64data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddressSetDataListenerResult {
    #[allow(dead_code)]
    addr: String,
    ret: String,
    err: Option<String>,
}

impl<R: Runtime> BtclassicSpp<R> {
    /* ---------- 无返回值调用 ---------- */
    pub fn start_scan(&self) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("startScan", ())
            .map_err(Into::into)
    }

    pub fn stop_scan(&self) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("stopScan", ())
            .map_err(Into::into)
    }

    pub fn start_ble_scan(&self) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("startBleScan", ())
            .map_err(Into::into)
    }

    pub fn stop_ble_scan(&self) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("stopBleScan", ())
            .map_err(Into::into)
    }

    pub fn connect(
        &self,
        addr: &str,
        remove_bond: bool,
        fallback_channels: &[u8],
    ) -> anyhow::Result<ConnectResult> {
        let arg = ConnectArg {
            addr: addr.to_owned(),
            remove_bond,
            fallback_channels: fallback_channels.to_vec(),
        };
        self.0.run_mobile_plugin("connect", arg).map_err(Into::into)
    }

    pub fn connect_ble(&self, addr: &str) -> anyhow::Result<ConnectResult> {
        let arg = ConnectArg {
            addr: addr.to_owned(),
            remove_bond: false,
            fallback_channels: Vec::new(),
        };
        self.0
            .run_mobile_plugin("connectBle", arg)
            .map_err(Into::into)
    }

    pub fn disconnect(&self, addr: &str) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("disconnect", AddressArg { addr })
            .map_err(Into::into)
    }

    pub fn disconnect_ble(&self, addr: &str) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("disconnectBle", AddressArg { addr })
            .map_err(Into::into)
    }

    pub fn start_subscription(&self, addr: &str) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("startSubscription", AddressArg { addr })
            .map_err(Into::into)
    }

    pub fn start_ble_subscription(&self, addr: &str) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>("startBleSubscription", AddressArg { addr })
            .map_err(Into::into)
    }

    pub fn send(&self, addr: &str, data: &[u8]) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>(
                "send",
                AddressSendPayload {
                    addr: addr.to_owned(),
                    b64data: general_purpose::STANDARD.encode(data),
                },
            )
            .map_err(Into::into)
    }

    pub fn send_ble(&self, addr: &str, data: &[u8]) -> anyhow::Result<()> {
        self.0
            .run_mobile_plugin::<()>(
                "sendBle",
                AddressSendPayload {
                    addr: addr.to_owned(),
                    b64data: general_purpose::STANDARD.encode(data),
                },
            )
            .map_err(Into::into)
    }

    /* ---------- 有返回值调用 ---------- */
    pub fn get_scanned_devices(&self) -> anyhow::Result<GetScannedDevicesResult> {
        self.0
            .run_mobile_plugin("getScannedDevices", ())
            .map_err(Into::into)
    }

    pub fn get_ble_scanned_devices(&self) -> anyhow::Result<GetScannedDevicesResult> {
        self.0
            .run_mobile_plugin("getBleScannedDevices", ())
            .map_err(Into::into)
    }

    pub fn get_connected_device_info(&self, addr: &str) -> anyhow::Result<SPPDevice> {
        self.0
            .run_mobile_plugin("getConnectedDeviceInfo", AddressArg { addr })
            .map_err(Into::into)
    }

    pub fn get_max_send_len(&self, addr: &str) -> anyhow::Result<Option<usize>> {
        let ret: GetMaxSendLenResult = self
            .0
            .run_mobile_plugin("getMaxSendLen", AddressArg { addr })
            .map_err(anyhow::Error::from)?;
        Ok(ret.ret)
    }

    pub fn get_ble_max_send_len(&self, addr: &str) -> anyhow::Result<Option<usize>> {
        let ret: GetMaxSendLenResult = self
            .0
            .run_mobile_plugin("getBleMaxSendLen", AddressArg { addr })
            .map_err(anyhow::Error::from)?;
        Ok(ret.ret)
    }

    /* ---------- 事件回调 ---------- */
    pub fn on_connected<F>(&self, addr: &str, cb: F) -> anyhow::Result<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let cb_arc = Arc::new(cb);
        let channel = Channel::<Value>::new(move |_raw| {
            (cb_arc)();
            Ok(())
        });
        self.0
            .run_mobile_plugin::<()>("onConnected", AddressChannelArg { addr, channel })?;
        Ok(())
    }

    pub fn set_data_listener<F>(&self, addr: &str, cb: F) -> anyhow::Result<()>
    where
        F: FnMut(Result<Vec<u8>, String>) + Send + 'static,
    {
        let cb_arc = Arc::new(Mutex::new(cb));
        let channel = Channel::<Value>::new({
            let cb_arc = Arc::clone(&cb_arc);
            move |raw: InvokeResponseBody| {
                let msg: AddressSetDataListenerResult = match raw.deserialize() {
                    Ok(m) => m,
                    Err(e) => {
                        if let Ok(mut f) = cb_arc.lock() {
                            (f)(Err(format!("Deserialize error from mobile: {}", e)));
                        }
                        return Ok(());
                    }
                };
                if let Some(err_msg) = msg.err {
                    if let Ok(mut f) = cb_arc.lock() {
                        (f)(Err(err_msg));
                    }
                    return Ok(());
                }
                let bytes = match general_purpose::STANDARD.decode(msg.ret) {
                    Ok(b) => b,
                    Err(e) => {
                        if let Ok(mut f) = cb_arc.lock() {
                            (f)(Err(format!("Base64 decode error from mobile: {}", e)));
                        }
                        return Ok(());
                    }
                };
                if let Ok(mut f) = cb_arc.lock() {
                    (f)(Ok(bytes));
                }
                Ok(())
            }
        });
        self.0
            .run_mobile_plugin::<()>("setDataListener", AddressChannelArg { addr, channel })?;
        Ok(())
    }
}
