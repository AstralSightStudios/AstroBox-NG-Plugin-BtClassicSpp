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
import java.util.concurrent.ConcurrentHashMap

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
    // Every address-scoped command gets a separate BTSpp, including its
    // socket/GATT, streams, callbacks, reader and send mutex.
    private val sessions = ConcurrentHashMap<String, BTSpp>()

    override fun load(webView: WebView) {
        implementation = BTSpp(activity, webView)
        this.webView = webView
    }

    private fun normalizeAddress(address: String): String {
        val raw = address.trim()
        val hex = raw.filter { it.isDigit() || it.lowercaseChar() in 'a'..'f' }
        if (hex.length >= 12) {
            return hex.takeLast(12)
                .chunked(2)
                .joinToString(":") { it.uppercase(java.util.Locale.US) }
        }
        return raw.uppercase(java.util.Locale.US)
    }

    private fun sessionFor(address: String): BTSpp {
        val key = normalizeAddress(address)
        return sessions.computeIfAbsent(key) {
            BTSpp(activity, webView)
        }
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
        val session = sessionFor(args.addr)
        webView.evaluateJavascript("console.log('Kotlin: Connecting to device ${args.addr}')", null)

        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = session.connect(
                activity,
                args.addr,
                args.remove_bond,
                args.fallback_channels.toList(),
            )
            if (isSuccessful) {
                invoke.resolve(JSObject().put("ret", true))
            } else {
                invoke.reject("CONNECT_ERROR", err ?: "Unknown error")
            }
        }
    }

    @Command
    fun connectBle(invoke: Invoke) {
        val args = invoke.parseArgs(ConnectArg::class.java)
        val session = sessionFor(args.addr)
        webView.evaluateJavascript("console.log('Kotlin: BLE connecting to device ${args.addr}')", null)

        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = session.connectBle(args.addr)
            if (isSuccessful) {
                invoke.resolve(JSObject().put("ret", true))
            } else {
                invoke.reject("BLE_CONNECT_ERROR", err ?: "Unknown error")
            }
        }
    }

    @Command
    fun disconnect(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        sessionFor(args.addr).disconnect()
        invoke.resolve()
    }

    @Command
    fun disconnectBle(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        sessionFor(args.addr).disconnectBle()
        invoke.resolve()
    }

    /** ------------ 连接成功回调 ------------ **/
    @Command
    fun onConnected(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressChannelArg::class.java)
        sessionFor(args.addr).onConnected {
            args.channel.send(JSObject().put("addr", args.addr))
        }
        invoke.resolve()
    }

    /** ------------ 连接信息 ------------ **/
    @SuppressLint("MissingPermission")
    @Command
    fun getConnectedDeviceInfo(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        val session = sessionFor(args.addr)
        val info = session.getConnectedDeviceInfo() ?: session.getBleConnectedDeviceInfo()
        val ret = JSObject().put("addr", args.addr)
        info?.let {
            ret.put("name", it.name)
            ret.put("address", it.address)
        }
        invoke.resolve(ret)
    }

    @Command
    fun getMaxSendLen(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        val ret = JSObject()
            .put("addr", args.addr)
            .put("ret", sessionFor(args.addr).getMaxSendLen())
        invoke.resolve(ret)
    }

    @Command
    fun getBleMaxSendLen(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        val ret = JSObject()
            .put("addr", args.addr)
            .put("ret", sessionFor(args.addr).getBleMaxSendLen())
        invoke.resolve(ret)
    }

    /** ------------ 数据监听 ------------ **/
    @Command
    fun setDataListener(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressChannelArg::class.java)
        val address = args.addr
        sessionFor(address).setDataListener(object : BTSpp.DataListener {
            override fun onDataReceived(data: ByteArray) {
                args.channel.send(
                    JSObject()
                        .put("addr", address)
                        .put("ret", Base64.encodeToString(data, Base64.NO_WRAP))
                )
            }

            override fun onError(e: IOException) {
                args.channel.send(
                    JSObject()
                        .put("addr", address)
                        .put("ret", "")
                        .put("err", e.toString())
                )
            }
        })
        invoke.resolve()
    }

    /** ------------ 开启订阅读取 ------------ **/
    @Command
    fun startSubscription(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        sessionFor(args.addr).startSubscription()
        invoke.resolve()
    }

    @Command
    fun startBleSubscription(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressArg::class.java)
        val session = sessionFor(args.addr)
        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = session.startBleSubscription()
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
        val args = invoke.parseArgs(RustTypes.AddressSendPayload::class.java)
        val data = Base64.decode(args.b64data, Base64.DEFAULT)
        sessionFor(args.addr).send(data)
        invoke.resolve()
    }

    @Command
    fun sendBle(invoke: Invoke) {
        val args = invoke.parseArgs(RustTypes.AddressSendPayload::class.java)
        val data = Base64.decode(args.b64data, Base64.DEFAULT)
        val session = sessionFor(args.addr)
        CoroutineScope(Dispatchers.IO).launch {
            val (isSuccessful, err) = session.sendBle(data)
            if (isSuccessful) {
                invoke.resolve()
            } else {
                invoke.reject("BLE_SEND_ERROR", err ?: "Unknown error")
            }
        }
    }
}
