package com.astralsight.astrobox.plugin.btclassic_spp

import android.annotation.SuppressLint
import android.app.Activity
import android.util.Base64
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.*
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import java.io.IOException

@InvokeArg
class ConnectArg {
    lateinit var addr: String
    var remove_bond: Boolean = true
    var fallback_channels: IntArray = intArrayOf()
}

@TauriPlugin
class BtClassicSPPPlugin(private val activity: Activity) : Plugin(activity) {
    private lateinit var implementation: BTSpp
    private lateinit var webView: WebView

    override fun load(webView: WebView) {
        implementation = BTSpp(activity, webView)
        implementation.initPermissions()
        this.webView = webView
    }

    override fun onResume() {
        implementation.onHostResume()
    }

    /** ------------ 蓝牙扫描 ------------ **/
    @SuppressLint("MissingPermission")
    @Command
    fun startScan(invoke: Invoke) {
        implementation.startScan()
        invoke.resolve()
    }

    @Command
    fun stopScan(invoke: Invoke) {
        implementation.stopScan()
        invoke.resolve()
    }

    @SuppressLint("MissingPermission")
    @Command
    fun getScannedDevices(invoke: Invoke) {
        val ret = JSArray()
        implementation.getScannedDevices().forEach { device ->
            val obj = JSObject()
            obj.put("name", device.name)
            obj.put("address", device.address)
            ret.put(obj)
        }
        invoke.resolve(JSObject().put("ret", ret))
    }

    /** ------------ BLE 扫描 ------------ **/
    @SuppressLint("MissingPermission")
    @Command
    fun startBleScan(invoke: Invoke) {
        implementation.startBleScan()
        invoke.resolve()
    }

    @Command
    fun stopBleScan(invoke: Invoke) {
        implementation.stopBleScan()
        invoke.resolve()
    }

    @Command
    fun getBleScannedDevices(invoke: Invoke) {
        val ret = JSArray()
        implementation.getBleScannedDevices().forEach { device ->
            val obj = JSObject()
            obj.put("name", device.name)
            obj.put("address", device.address)
            ret.put(obj)
        }
        invoke.resolve(JSObject().put("ret", ret))
    }

    /** ------------ 连接 ------------ **/
    @SuppressLint("MissingPermission")
    @Command
    fun connect(invoke: Invoke) {
        val args = invoke.parseArgs(ConnectArg::class.java)
        webView.evaluateJavascript("console.log('Kotlin: Connecting to device ${args.addr}')", null)

        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = implementation.connect(
                activity,
                args.addr,
                args.remove_bond,
                args.fallback_channels.toList(),
            )
            if (isSuccessful) {
                val ret = JSObject()
                ret.put("ret", true)
                invoke.resolve(ret)
            } else {
                invoke.reject("CONNECT_ERROR", err ?: "Unknown error")
            }
        }
    }

    @Command
    fun connectBle(invoke: Invoke) {
        val args = invoke.parseArgs(ConnectArg::class.java)
        webView.evaluateJavascript("console.log('Kotlin: BLE connecting to device ${args.addr}')", null)

        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = implementation.connectBle(args.addr)
            if (isSuccessful) {
                invoke.resolve(JSObject().put("ret", true))
            } else {
                invoke.reject("BLE_CONNECT_ERROR", err ?: "Unknown error")
            }
        }
    }

    @Command
    fun disconnect(invoke: Invoke) {
        implementation.disconnect()
        invoke.resolve()
    }

    @Command
    fun disconnectBle(invoke: Invoke) {
        implementation.disconnectBle()
        invoke.resolve()
    }

    /** ------------ 连接成功回调 ------------ **/
    @Command
    fun onConnected(invoke: Invoke) {
        val out = invoke.parseArgs(Channel::class.java)
        implementation.onConnected { out.send(JSObject()) }
        invoke.resolve()
    }

    /** ------------ 连接信息 ------------ **/
    @SuppressLint("MissingPermission")
    @Command
    fun getConnectedDeviceInfo(invoke: Invoke) {
        val info = implementation.getConnectedDeviceInfo()
        val ret = JSObject()
        info?.let {
            ret.put("name", it.name)
            ret.put("address", it.address)
        }
        invoke.resolve(ret)
    }

    @Command
    fun getMaxSendLen(invoke: Invoke) {
        val ret = JSObject()
        ret.put("ret", implementation.getMaxSendLen())
        invoke.resolve(ret)
    }

    @Command
    fun getBleMaxSendLen(invoke: Invoke) {
        val ret = JSObject()
        ret.put("ret", implementation.getBleMaxSendLen())
        invoke.resolve(ret)
    }

    /** ------------ 数据监听 ------------ **/
    @Command
    fun setDataListener(invoke: Invoke) {
        val out = invoke.parseArgs(Channel::class.java)
        val callbackData = JSObject()
        implementation.setDataListener(object : BTSpp.DataListener {
            override fun onDataReceived(data: ByteArray) {
                callbackData.put("ret", Base64.encodeToString(data, Base64.NO_WRAP))
                out.send(callbackData)
            }

            override fun onError(e: IOException) {
                callbackData.put("ret", "")
                callbackData.put("err", e.toString())
                out.send(callbackData)
            }
        })
        invoke.resolve()
    }

    /** ------------ 开启订阅读取 ------------ **/
    @Command
    fun startSubscription(invoke: Invoke) {
        implementation.startSubscription()
        invoke.resolve()
    }

    @Command
    fun startBleSubscription(invoke: Invoke) {
        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = implementation.startBleSubscription()
            if (isSuccessful) {
                invoke.resolve()
            } else {
                invoke.reject("BLE_SUBSCRIBE_ERROR", err ?: "Unknown error")
            }
        }
    }

    /** ------------ 发送 ------------ **/
    @Command
    fun send(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.SPPSendPayload::class.java)
        val data = Base64.decode(args.b64data, Base64.DEFAULT)
        implementation.send(data)
        invoke.resolve()
    }

    @Command
    fun sendBle(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.SPPSendPayload::class.java)
        val data = Base64.decode(args.b64data, Base64.DEFAULT)
        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = implementation.sendBle(data)
            if (isSuccessful) {
                invoke.resolve()
            } else {
                invoke.reject("BLE_SEND_ERROR", err ?: "Unknown error")
            }
        }
    }
}
