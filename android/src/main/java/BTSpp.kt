package com.astralsight.astrobox.plugin.btclassic_spp

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.BluetoothStatusCodes
import android.bluetooth.BluetoothSocket
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Context.RECEIVER_EXPORTED
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.webkit.WebView
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.SendChannel
import kotlinx.coroutines.channels.actor
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.Locale
import java.util.UUID
import java.util.concurrent.ConcurrentLinkedQueue
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

@OptIn(ObsoleteCoroutinesApi::class)
class BTSpp(private val context: Context, private val webView: WebView) {

    private val SPP_PREFIX = "00001101"
    private val BLE_UUID_KEYWORD_XIAOMI_SERVICE = "0050"
    private val BLE_UUID_KEYWORD_XIAOMI_SENT = "005f"
    private val BLE_UUID_KEYWORD_XIAOMI_RECV = "005e"
    private val BLE_UUID_VIVO_SERVICE = "0000276008c211e190730e8ac72e1011"
    private val BLE_UUID_VIVO_SENT = "0000276008c211e190730e8ac72e0011"
    private val BLE_UUID_VIVO_RECV = "0000276008c211e190730e8ac72e0012"
    private val VIVO_MANUFACTURER_ID = 2103
    private val BLE_DEFAULT_MTU = 23
    private val BLE_REQUESTED_MTU = 247
    private val BLE_WRITE_DELAY_MS = 12L
    /** GATT 一次只能有一个操作在途，写入起不来多半是上一个操作还没回调；短暂等一下再试 */
    private val BLE_WRITE_BUSY_RETRIES = 50
    private val BLE_WRITE_BUSY_RETRY_DELAY_MS = 10L
    /** 读服务特征的等待上限：需要加密时系统会先弹配对，读请求压到配对结束才回来 */
    private val BLE_SERVICE_PROBE_TIMEOUT_MS = 8_000L
    /** GATT 连接立刻失败（典型是 status 133）时再试的次数；超时不重试 */
    private val BLE_CONNECT_ATTEMPTS = 2
    private val BLE_CONNECT_RETRY_DELAY_MS = 600L
    private val UNPAIR_SETTLE_TIMEOUT_MS = 3_000L
    private val CLIENT_CHARACTERISTIC_CONFIG_UUID =
        UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
    private val STREAM_WRITE_HINT = 60 * 1024
    private val PERMISSION_REQUEST_CODE = 1001
    private val PERMISSION_REQUEST_COOLDOWN_MS = 1500L
    private val PRECISE_LOCATION_REQUIRED_MESSAGE =
        "请授予AstroBox访问您的位置，否则将无法扫描蓝牙设备，此为安卓系统要求。"
    private val BLUETOOTH_PERMISSION_REQUIRED_MESSAGE =
        "请授予AstroBox访问附近设备的权限，否则将无法连接到任何蓝牙设备。"
    private val PERMISSION_DIALOG_COOLDOWN_MS = 3000L
    @Volatile private var lastPermissionRequestAtMs: Long = 0L
    @Volatile private var lastPermissionDialogAtMs: Long = 0L
    @Volatile private var hasRequestedRuntimePermissions: Boolean = false

    private val adapter: BluetoothAdapter? = BluetoothAdapter.getDefaultAdapter()
    private val scannedDevices = mutableListOf<BluetoothDevice>()
    data class BleScannedDevice(val name: String?, val address: String)
    private data class VivoAdvertisementInfo(
        val protocolVersion: Int,
        val mask: Int,
        val productId: Int,
        val mac: String,
    )
    private val bleScannedDevices = mutableListOf<BleScannedDevice>()
    private val bleDeviceCache = mutableMapOf<String, BluetoothDevice>()
    private var bleScanCallback: ScanCallback? = null

    private var socket: BluetoothSocket? = null
    private var inStream: InputStream? = null
    private var outStream: OutputStream? = null
    private var connectedDevice: BluetoothDevice? = null

    private var bleGatt: BluetoothGatt? = null
    private var bleConnectedDevice: BluetoothDevice? = null
    private var bleWriteCharacteristic: BluetoothGattCharacteristic? = null
    private var bleNotifyCharacteristic: BluetoothGattCharacteristic? = null
    private var bleServiceProbeCharacteristic: BluetoothGattCharacteristic? = null
    @Volatile private var bleMtu: Int = BLE_DEFAULT_MTU
    private var bleConnectDeferred: CompletableDeferred<Boolean>? = null
    private var bleServicesDeferred: CompletableDeferred<Boolean>? = null
    private var bleMtuDeferred: CompletableDeferred<Int>? = null
    private var bleDescriptorWriteDeferred: CompletableDeferred<Boolean>? = null
    private var bleReadDeferred: CompletableDeferred<Boolean>? = null
    private val bleSendMutex = Mutex()
    @Volatile private var bleManualDisconnect: Boolean = false

    private val sendScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var sendActor: SendChannel<ByteArray>? = null
    private val pendingPool = ConcurrentLinkedQueue<ByteArray>()

    @Volatile private var readThread: Thread? = null
    private val uiHandler = Handler(Looper.getMainLooper())

    private fun requiredRuntimePermissions(forScan: Boolean): Array<String> {
        return when {
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> {
                val permissions = mutableListOf<String>()
                if (forScan) permissions += Manifest.permission.BLUETOOTH_SCAN
                permissions += Manifest.permission.BLUETOOTH_CONNECT
                permissions.toTypedArray()
            }
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q -> {
                if (forScan) arrayOf(Manifest.permission.ACCESS_FINE_LOCATION) else emptyArray()
            }
            else -> {
                if (forScan) arrayOf(Manifest.permission.ACCESS_COARSE_LOCATION) else emptyArray()
            }
        }
    }

    private fun missingRuntimePermissions(activity: Activity, forScan: Boolean): Array<String> {
        return requiredRuntimePermissions(forScan)
            .distinct()
            .filter {
                ContextCompat.checkSelfPermission(activity, it) != PackageManager.PERMISSION_GRANTED
            }
            .toTypedArray()
    }

    fun missingRuntimePermissions(forScan: Boolean): Array<String> {
        val activity = context as? Activity
            ?: throw IllegalStateException("需要传入 Activity 作为 context，才能检查运行时权限。")
        return missingRuntimePermissions(activity, forScan)
    }

    fun hasRuntimePermissions(forScan: Boolean): Boolean =
        missingRuntimePermissions(forScan).isEmpty()

    private fun hasLocationPermission(activity: Activity): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) return true
        val permission = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            Manifest.permission.ACCESS_FINE_LOCATION
        } else {
            Manifest.permission.ACCESS_COARSE_LOCATION
        }
        return ContextCompat.checkSelfPermission(activity, permission) ==
            PackageManager.PERMISSION_GRANTED
    }

    private fun ensureRuntimePermissions(requestIfMissing: Boolean, forScan: Boolean): Boolean {
        val activity = context as? Activity
            ?: throw IllegalStateException("需要传入 Activity 作为 context，才能申请运行时权限。")
        val missing = missingRuntimePermissions(activity, forScan)
        if (missing.isEmpty()) return true
        if (!requestIfMissing) return false
        val now = System.currentTimeMillis()
        if (now - lastPermissionRequestAtMs < PERMISSION_REQUEST_COOLDOWN_MS) {
            return false
        }
        lastPermissionRequestAtMs = now
        hasRequestedRuntimePermissions = true

        val request = {
            ActivityCompat.requestPermissions(activity, missing, PERMISSION_REQUEST_CODE)
        }
        if (Looper.myLooper() == Looper.getMainLooper()) {
            request()
        } else {
            uiHandler.post { request() }
        }
        return false
    }

    /** Request permissions only after a user-initiated Bluetooth operation. */
    private fun ensureRuntimePermissionsForUse(forScan: Boolean): Boolean {
        val activity = context as? Activity
            ?: throw IllegalStateException("需要传入 Activity 作为 context，才能申请运行时权限。")
        if (ensureRuntimePermissions(requestIfMissing = false, forScan = forScan)) return true

        val missing = missingRuntimePermissions(activity, forScan)
        if (missing.isEmpty()) return true

        val now = System.currentTimeMillis()
        if (now - lastPermissionRequestAtMs < PERMISSION_REQUEST_COOLDOWN_MS) {
            return false
        }

        val canRequestAgain = !hasRequestedRuntimePermissions || missing.any {
            ActivityCompat.shouldShowRequestPermissionRationale(activity, it)
        }
        if (canRequestAgain) {
            ensureRuntimePermissions(requestIfMissing = true, forScan = forScan)
        } else {
            showPermissionRequiredDialogIfNeeded(forScan)
        }
        return false
    }

    private fun showPermissionRequiredDialogIfNeeded(forScan: Boolean) {
        val activity = context as? Activity ?: return
        val missing = missingRuntimePermissions(activity, forScan)
        if (missing.isEmpty()) return
        val now = System.currentTimeMillis()
        if (now - lastPermissionDialogAtMs < PERMISSION_DIALOG_COOLDOWN_MS) {
            return
        }
        lastPermissionDialogAtMs = now

        val message = if (forScan && !hasLocationPermission(activity)) {
            PRECISE_LOCATION_REQUIRED_MESSAGE
        } else {
            BLUETOOTH_PERMISSION_REQUIRED_MESSAGE
        }
        val showDialog = {
            android.app.AlertDialog.Builder(activity)
                .setMessage(message)
                .setCancelable(true)
                .setPositiveButton("去设置") { _, _ ->
                    val intent = Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).apply {
                        data = Uri.fromParts("package", activity.packageName, null)
                    }
                    activity.startActivity(intent)
                }
                .setNegativeButton("稍后", null)
                .show()
        }

        if (Looper.myLooper() == Looper.getMainLooper()) {
            showDialog()
        } else {
            uiHandler.post { showDialog() }
        }
    }

    private fun missingPermissionsMessage(forScan: Boolean): String {
        val activity = context as? Activity
            ?: return BLUETOOTH_PERMISSION_REQUIRED_MESSAGE
        if (forScan && !hasLocationPermission(activity)) {
            return PRECISE_LOCATION_REQUIRED_MESSAGE
        }
        val missing = missingRuntimePermissions(activity, forScan)
        if (missing.isEmpty()) return BLUETOOTH_PERMISSION_REQUIRED_MESSAGE
        return "Missing permissions: ${missing.joinToString(", ")}"
    }

    interface DataListener {
        fun onDataReceived(data: ByteArray)
        fun onError(e: IOException)
    }
    private var dataListener: DataListener? = null
    private var onConnectedCallback: (() -> Unit)? = null

    fun getScannedDevices(): List<BluetoothDevice> = scannedDevices.toList()
    fun getConnectedDeviceInfo(): BluetoothDevice? = connectedDevice
    fun getMaxSendLen(): Int? = if (connectedDevice != null) STREAM_WRITE_HINT else null
    fun getBleScannedDevices(): List<BleScannedDevice> = synchronized(bleScannedDevices) {
        bleScannedDevices.toList()
    }
    fun getBleConnectedDeviceInfo(): BluetoothDevice? = bleConnectedDevice
    fun getBleMaxSendLen(): Int? {
        if (bleGatt == null || bleWriteCharacteristic == null) return null
        return (bleMtu - 3).coerceAtLeast(20)
    }
    fun setDataListener(listener: DataListener) { dataListener = listener }

    private suspend fun webViewLog(content: String) {
        withContext(Dispatchers.Main) {
            webView.evaluateJavascript("console.log('$content')", null)
        }
    }

    private fun normalizeBluetoothAddress(raw: String): String {
        val hex = raw.filter { it.isDigit() || it.lowercaseChar() in 'a'..'f' }
        if (hex.length >= 12) {
            return hex.takeLast(12)
                .chunked(2)
                .joinToString(":") { it.uppercase(Locale.US) }
        }
        return raw.trim().uppercase(Locale.US)
    }

    private fun uuidCompact(uuid: UUID): String =
        uuid.toString().replace("-", "").lowercase(Locale.US)

    private fun uuidContains(uuid: UUID, keyword: String): Boolean =
        uuidCompact(uuid).contains(keyword.lowercase(Locale.US))

    private fun uuidCompactEq(uuid: UUID, expected: String): Boolean =
        uuidCompact(uuid) == expected.lowercase(Locale.US)

    private fun isVivoWatchName(name: String?): Boolean {
        val normalized = name?.trim()?.lowercase(Locale.US) ?: return false
        return normalized.startsWith("vivo watch") || normalized.startsWith("iqoo watch")
    }

    private fun parseVivoManufacturerData(data: ByteArray?): VivoAdvertisementInfo? {
        if (data == null || data.size < 12 || data[0].toInt() != 0) return null
        val protocolVersion = data[1].toInt() and 0xff
        val mask = (data[2].toInt() and 0xff) or ((data[3].toInt() and 0xff) shl 8)
        val productId = (data[4].toInt() and 0xff) or ((data[5].toInt() and 0xff) shl 8)
        val macBytes = data.copyOfRange(6, 12)
        if (macBytes.all { it.toInt() == 0 }) return null
        val mac = macBytes.joinToString(":") { "%02X".format(it.toInt() and 0xff) }
        return VivoAdvertisementInfo(protocolVersion, mask, productId, mac)
    }

    @SuppressLint("MissingPermission")
    private fun rememberBleScanResult(result: ScanResult) {
        val record = result.scanRecord
        val device = result.device ?: return
        val vivoInfo = parseVivoManufacturerData(
            record?.manufacturerSpecificData?.get(VIVO_MANUFACTURER_ID)
        )
        val name = record?.deviceName ?: runCatching { device.name }.getOrNull().orEmpty()
        val emitAddress = when {
            vivoInfo != null -> {
                uiHandler.post {
                    webView.evaluateJavascript(
                        "console.log('Kotlin BLE: vivo advertisement raw=${device.address} mac=${vivoInfo.mac} product=${vivoInfo.productId} mask=${vivoInfo.mask} protocol=${vivoInfo.protocolVersion}')",
                        null
                    )
                }
                vivoInfo.mac
            }
            isVivoWatchName(name) -> device.address
            else -> return
        }

        val key = normalizeBluetoothAddress(emitAddress)
        if (key.isEmpty()) return

        synchronized(bleScannedDevices) {
            if (bleScannedDevices.any { normalizeBluetoothAddress(it.address) == key }) return
            bleScannedDevices.add(BleScannedDevice(name.ifEmpty { null }, emitAddress))
        }
        synchronized(bleDeviceCache) {
            bleDeviceCache[key] = device
            bleDeviceCache[normalizeBluetoothAddress(device.address)] = device
        }
    }

    @SuppressLint("MissingPermission")
    fun startScan(): Boolean {
        if (!hasRuntimePermissions(forScan = true)) return false
        val bt = adapter ?: return false

        scannedDevices.clear()
        if (bt.isDiscovering) bt.cancelDiscovery()

        val filter = IntentFilter(BluetoothDevice.ACTION_FOUND)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(scanReceiver, filter, RECEIVER_EXPORTED)
        } else {
            context.registerReceiver(scanReceiver, filter)
        }
        return bt.startDiscovery()
    }

    @SuppressLint("MissingPermission")
    fun stopScan() {
        adapter?.cancelDiscovery()
        try { context.unregisterReceiver(scanReceiver) } catch (_: IllegalArgumentException) {}
    }

    @SuppressLint("MissingPermission")
    fun startBleScan(): Boolean {
        if (!hasRuntimePermissions(forScan = true)) return false

        stopBleScan()
        synchronized(bleScannedDevices) { bleScannedDevices.clear() }
        synchronized(bleDeviceCache) { bleDeviceCache.clear() }

        val scanner = adapter?.bluetoothLeScanner ?: return false
        val callback = object : ScanCallback() {
            override fun onScanResult(callbackType: Int, result: ScanResult) {
                rememberBleScanResult(result)
            }

            override fun onBatchScanResults(results: MutableList<ScanResult>) {
                results.forEach(::rememberBleScanResult)
            }

            override fun onScanFailed(errorCode: Int) {
                uiHandler.post {
                    webView.evaluateJavascript(
                        "console.warn('Kotlin BLE: scan failed code=$errorCode')",
                        null
                    )
                }
            }
        }

        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .build()
        bleScanCallback = callback
        scanner.startScan(emptyList<ScanFilter>(), settings, callback)
        return true
    }

    @SuppressLint("MissingPermission")
    fun stopBleScan() {
        val callback = bleScanCallback ?: return
        bleScanCallback = null
        runCatching {
            adapter?.bluetoothLeScanner?.stopScan(callback)
        }
    }

    @SuppressLint("MissingPermission")
    private suspend fun unpairDevice(device: BluetoothDevice): Boolean {
        return withContext(Dispatchers.IO) {
            try {
                webViewLog("Kotlin: Attempting to unpair device ${device.address} via reflection.")
                if (device.bondState == BluetoothDevice.BOND_NONE) {
                    return@withContext true
                }
                val method = device.javaClass.getMethod("removeBond")
                val result = method.invoke(device) as? Boolean ?: false
                if (result) {
                    webViewLog("Kotlin: removeBond() invoked successfully.")
                    // removeBond 只是发起解绑，系统要过一会儿才真正清掉配对。紧接着就连的话，
                    // 一部分 ROM 上 RFCOMM 会撞上正在拆的链路直接失败，或者再弹一次配对。
                    val unbonded = withTimeoutOrNull(UNPAIR_SETTLE_TIMEOUT_MS) {
                        while (device.bondState != BluetoothDevice.BOND_NONE) delay(100)
                        true
                    } == true
                    if (!unbonded) {
                        webViewLog("Kotlin: bond still present after removeBond(); connecting anyway.")
                    }
                } else {
                    webViewLog("Kotlin: removeBond() invocation failed.")
                }
                result
            } catch (e: Exception) {
                webViewLog("Kotlin: Reflection failed for removeBond: ${e.message}")
                false
            }
        }
    }

    suspend fun connect(
        context: Context,
        address: String,
        remove_bond: Boolean,
        fallbackChannels: List<Int> = emptyList()
    ): Pair<Boolean, String?> = withContext(Dispatchers.IO) {
        var errMsg: String?
        try {
            if (!ensureRuntimePermissionsForUse(forScan = false)) {
                errMsg = missingPermissionsMessage(forScan = false)
                return@withContext false to errMsg
            }

            // Reconnecting this session must not leave its old actor/read thread
            // racing with the new socket. Other addresses use other BTSpp
            // instances and are intentionally untouched.
            if (socket != null || connectedDevice != null) {
                val pendingOnConnected = onConnectedCallback
                disconnect()
                onConnectedCallback = pendingOnConnected
            }

            val adapter = (context.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager).adapter
                ?: return@withContext false to "BluetoothAdapter == null"
            val dev: BluetoothDevice = try {
                adapter.getRemoteDevice(address)
            } catch (iae: IllegalArgumentException) {
                errMsg = "Invalid MAC address: ${iae.message}"
                return@withContext false to errMsg
            }

            if (adapter.isDiscovering) adapter.cancelDiscovery()

            if (remove_bond) {
                unpairDevice(dev)
            }

            var sock = trySdpUuid(dev)
            if (sock == null) {
                val channels = fallbackChannels.ifEmpty { listOf(5, 1) }.distinct()
                for (channel in channels) {
                    sock = tryChannel(dev, channel, if (channel == 5) 8_000 else 6_000)
                    if (sock != null) break
                }
            }
            val connectedSock = sock ?: return@withContext false to "No SPP channel/UUID available"

            socket = connectedSock
            inStream = connectedSock.inputStream
            outStream = connectedSock.outputStream
            connectedDevice = dev

            sendActor = sendScope.actor(capacity = Channel.UNLIMITED) {
                for (payload in channel) {
                    try {
                        outStream?.write(payload)
                        outStream?.flush()
                    } catch (e: IOException) {
                        val listener = dataListener
                        uiHandler.post { listener?.onError(e) }
                        break
                    }
                }
            }

            while (true) {
                val pending = pendingPool.poll() ?: break
                sendActor?.trySend(pending)
            }

            onConnectedCallback?.let { cb ->
                uiHandler.post { cb() }
                onConnectedCallback = null
            }
            true to null
        } catch (e: Exception) {
            errMsg = e.message
            webViewLog("Connect failed: $errMsg")
            false to errMsg
        }
    }

    fun onConnected(cb: () -> Unit) {
        if (connectedDevice != null || bleConnectedDevice != null) {
            uiHandler.post { cb() }
        } else {
            onConnectedCallback = cb
        }
    }

    /**
     * 阻塞的 `BluetoothSocket.connect()` 带超时。`withTimeout` 打断不了阻塞调用（它只在挂起点检查
     * 取消），以前写的 6 秒超时实际要等系统自己放弃（十几秒以上）。到点直接关掉 socket，
     * `connect()` 会立刻抛 IOException 返回。
     */
    private suspend fun connectWithTimeout(sock: BluetoothSocket, timeoutMs: Long) {
        coroutineScope {
            // 显式放到 IO 线程池：调用方线程会被 connect() 阻塞住
            val watchdog = launch(Dispatchers.IO) {
                delay(timeoutMs)
                runCatching { sock.close() }
            }
            try {
                sock.connect()
            } finally {
                watchdog.cancel()
            }
        }
        if (!sock.isConnected) {
            throw IOException("RFCOMM connect timed out after ${timeoutMs}ms")
        }
    }

    /** 通过 SDP UUID 尝试（insecure 一次 + secure 一次） **/
    @SuppressLint("MissingPermission")
    private suspend fun trySdpUuid(dev: BluetoothDevice): BluetoothSocket? {
        if (!dev.fetchUuidsWithSdp()) return null

        // 只是等 SDP 结果回来（最多 2 秒），不是重试连接。以前每一轮都把 insecure + secure 各连一次，
        // 连不上就转 20 轮：最坏要好几分钟，而且每次 secure 连接都可能让系统再弹一次配对请求。
        var uuid: UUID? = null
        for (attempt in 0 until 20) {
            uuid = dev.uuids
                ?.firstOrNull { it.uuid.toString().startsWith(SPP_PREFIX, ignoreCase = true) }
                ?.uuid
            if (uuid != null) break
            delay(100)
        }
        val sppUuid = uuid ?: return null

        /* insecure 优先，部分国产 ROM 只允许 insecure 连接 */
        val insecureResult = runCatching {
            webViewLog("Kotlin: trySdpUuid (insecure, uuid=$sppUuid)")
            val sock = dev.createInsecureRfcommSocketToServiceRecord(sppUuid)
            try {
                connectWithTimeout(sock, 6_000)
                sock
            } catch (e: Exception) {
                runCatching { sock.close() }
                throw e
            }
        }
        if (insecureResult.isSuccess) {
            return insecureResult.getOrNull()
        }
        webViewLog("Kotlin: insecure failed (${insecureResult.exceptionOrNull()?.message}), fallback secure")

        val secureResult = runCatching {
            val sock = dev.createRfcommSocketToServiceRecord(sppUuid) // secure
            try {
                connectWithTimeout(sock, 6_000)
                sock
            } catch (e: Exception) {
                runCatching { sock.close() }
                throw e
            }
        }
        if (secureResult.isSuccess) {
            return secureResult.getOrNull()
        }
        webViewLog("Kotlin: secure failed (${secureResult.exceptionOrNull()?.message})")
        return null
    }

    /** 通过 channel 号反射尝试（secure + insecure） **/
    private suspend fun tryChannel(
        dev: BluetoothDevice,
        ch: Int,
        timeoutMs: Long
    ): BluetoothSocket? {
        return runCatching {
            webViewLog("Kotlin: tryChannel(ch=$ch)")
            val method = runCatching {
                dev.javaClass.getMethod("createInsecureRfcommSocket", Int::class.javaPrimitiveType)
            }.getOrNull() ?: dev.javaClass.getMethod("createRfcommSocket", Int::class.javaPrimitiveType)

            val sock = method.invoke(dev, ch) as BluetoothSocket
            try {
                connectWithTimeout(sock, timeoutMs)
                sock
            } catch (e: Exception) {
                runCatching { sock.close() }
                throw e
            }
        }.getOrNull()
    }

    private fun emitBleData(data: ByteArray) {
        val bytes = data.copyOf()
        uiHandler.post { dataListener?.onDataReceived(bytes) }
    }

    private val bleGattCallback = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
            if (gatt !== bleGatt) return
            when (newState) {
                BluetoothProfile.STATE_CONNECTED -> {
                    bleConnectDeferred?.complete(status == BluetoothGatt.GATT_SUCCESS)
                    bleConnectDeferred = null
                }
                BluetoothProfile.STATE_DISCONNECTED -> {
                    bleConnectDeferred?.complete(false)
                    bleConnectDeferred = null
                    if (!bleManualDisconnect && bleConnectedDevice != null) {
                        val listener = dataListener
                        uiHandler.post {
                            listener?.onError(IOException("BLE disconnected: status=$status"))
                        }
                    }
                    bleConnectedDevice = null
                }
            }
        }

        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            if (gatt !== bleGatt) return
            bleServicesDeferred?.complete(status == BluetoothGatt.GATT_SUCCESS)
            bleServicesDeferred = null
        }

        override fun onMtuChanged(gatt: BluetoothGatt, mtu: Int, status: Int) {
            if (gatt !== bleGatt) return
            val acceptedMtu = if (status == BluetoothGatt.GATT_SUCCESS) mtu else BLE_DEFAULT_MTU
            bleMtu = acceptedMtu
            bleMtuDeferred?.complete(acceptedMtu)
            bleMtuDeferred = null
        }

        override fun onDescriptorWrite(
            gatt: BluetoothGatt,
            descriptor: BluetoothGattDescriptor,
            status: Int
        ) {
            if (gatt !== bleGatt) return
            bleDescriptorWriteDeferred?.complete(status == BluetoothGatt.GATT_SUCCESS)
            bleDescriptorWriteDeferred = null
        }

        @Deprecated("Deprecated in Java")
        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            status: Int
        ) {
            if (gatt !== bleGatt) return
            bleReadDeferred?.complete(status == BluetoothGatt.GATT_SUCCESS)
            bleReadDeferred = null
        }

        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int
        ) {
            if (gatt !== bleGatt) return
            bleReadDeferred?.complete(status == BluetoothGatt.GATT_SUCCESS)
            bleReadDeferred = null
        }

        @Deprecated("Deprecated in Java")
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic
        ) {
            if (gatt !== bleGatt) return
            characteristic.value?.let(::emitBleData)
        }

        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray
        ) {
            if (gatt !== bleGatt) return
            emitBleData(value)
        }
    }

    @SuppressLint("MissingPermission")
    private suspend fun resolveBleDevice(address: String): BluetoothDevice? {
        val key = normalizeBluetoothAddress(address)
        synchronized(bleDeviceCache) {
            bleDeviceCache[key]?.let { return it }
        }

        val directDevice = runCatching { adapter?.getRemoteDevice(address) }.getOrNull()
        startBleScan()
        val deadline = System.currentTimeMillis() + 12_000L
        while (System.currentTimeMillis() < deadline) {
            synchronized(bleDeviceCache) {
                bleDeviceCache[key]?.let {
                    stopBleScan()
                    return it
                }
            }
            delay(150)
        }
        stopBleScan()
        return directDevice
    }

    @SuppressLint("MissingPermission")
    private suspend fun requestBleMtu(gatt: BluetoothGatt): Int {
        val deferred = CompletableDeferred<Int>()
        bleMtuDeferred = deferred
        if (!gatt.requestMtu(BLE_REQUESTED_MTU)) {
            bleMtuDeferred = null
            return BLE_DEFAULT_MTU
        }
        return withTimeoutOrNull(4_000L) { deferred.await() } ?: BLE_DEFAULT_MTU
    }

    @SuppressLint("MissingPermission")
    private suspend fun discoverBleServices(gatt: BluetoothGatt): Boolean {
        val deferred = CompletableDeferred<Boolean>()
        bleServicesDeferred = deferred
        if (!gatt.discoverServices()) {
            bleServicesDeferred = null
            return false
        }
        return withTimeoutOrNull(10_000L) { deferred.await() } == true
    }

    private fun extractBleCharacteristics(services: List<BluetoothGattService>): Boolean {
        var recv: BluetoothGattCharacteristic? = null
        var sent: BluetoothGattCharacteristic? = null
        var serviceProbe: BluetoothGattCharacteristic? = null

        for (service in services) {
            val serviceUuid = service.uuid
            val isXiaomi = uuidContains(serviceUuid, "fe95")
            val isVivo = uuidCompactEq(serviceUuid, BLE_UUID_VIVO_SERVICE)
            if (!isXiaomi && !isVivo) continue

            for (characteristic in service.characteristics) {
                val characteristicUuid = characteristic.uuid
                when {
                    isXiaomi && uuidContains(characteristicUuid, BLE_UUID_KEYWORD_XIAOMI_RECV) ->
                        recv = characteristic
                    isXiaomi && uuidContains(characteristicUuid, BLE_UUID_KEYWORD_XIAOMI_SENT) ->
                        sent = characteristic
                    isXiaomi && uuidContains(characteristicUuid, BLE_UUID_KEYWORD_XIAOMI_SERVICE) ->
                        serviceProbe = characteristic
                    isVivo && uuidCompactEq(characteristicUuid, BLE_UUID_VIVO_RECV) ->
                        recv = characteristic
                    isVivo && uuidCompactEq(characteristicUuid, BLE_UUID_VIVO_SENT) ->
                        sent = characteristic
                }
            }
        }

        bleNotifyCharacteristic = recv
        bleWriteCharacteristic = sent
        bleServiceProbeCharacteristic = serviceProbe
        return recv != null && sent != null
    }

    @SuppressLint("MissingPermission")
    suspend fun connectBle(address: String): Pair<Boolean, String?> = withContext(Dispatchers.IO) {
        try {
            if (!ensureRuntimePermissionsForUse(forScan = true)) {
                return@withContext false to missingPermissionsMessage(forScan = true)
            }

            val dev = resolveBleDevice(address)
                ?: return@withContext false to "BLE device not found: $address"

            adapter?.cancelDiscovery()
            val pendingOnConnected = onConnectedCallback
            disconnectBle()
            onConnectedCallback = pendingOnConnected
            bleManualDisconnect = false

            // GATT 连接立刻失败（status 133 这类协议栈错误，安卓上很常见）时关掉这个 gatt 稍等再试一次；
            // 15 秒都没连上说明设备不在附近，不再重试。
            var connectedGatt: BluetoothGatt? = null
            for (attempt in 1..BLE_CONNECT_ATTEMPTS) {
                val connectedDeferred = CompletableDeferred<Boolean>()
                bleConnectDeferred = connectedDeferred
                val candidate = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                    dev.connectGatt(context, false, bleGattCallback, BluetoothDevice.TRANSPORT_LE)
                } else {
                    dev.connectGatt(context, false, bleGattCallback)
                } ?: return@withContext false to "connectGatt returned null"

                bleGatt = candidate
                val result = withTimeoutOrNull(15_000L) { connectedDeferred.await() }
                if (result == true) {
                    connectedGatt = candidate
                    break
                }
                val pending = onConnectedCallback
                disconnectBle()
                onConnectedCallback = pending
                if (result == null || attempt == BLE_CONNECT_ATTEMPTS) {
                    onConnectedCallback = null
                    return@withContext false to "BLE GATT connection timed out or failed"
                }
                webViewLog("Kotlin: BLE GATT connection failed immediately; retrying ($attempt/$BLE_CONNECT_ATTEMPTS)")
                delay(BLE_CONNECT_RETRY_DELAY_MS)
            }
            val gatt = connectedGatt
                ?: return@withContext false to "BLE GATT connection timed out or failed"

            bleMtu = requestBleMtu(gatt)

            if (!discoverBleServices(gatt)) {
                disconnectBle()
                return@withContext false to "BLE service discovery failed"
            }

            if (!extractBleCharacteristics(gatt.services)) {
                disconnectBle()
                return@withContext false to "BLE VSCP characteristics not found"
            }

            bleConnectedDevice = dev
            onConnectedCallback?.let { cb ->
                uiHandler.post { cb() }
                onConnectedCallback = null
            }
            true to null
        } catch (e: Exception) {
            disconnectBle()
            false to (e.message ?: e.toString())
        }
    }

    @SuppressLint("MissingPermission")
    suspend fun startBleSubscription(): Pair<Boolean, String?> = withContext(Dispatchers.IO) {
        val gatt = bleGatt ?: return@withContext false to "BLE GATT not connected"
        val characteristic = bleNotifyCharacteristic
            ?: return@withContext false to "BLE notify characteristic not found"

        if (!gatt.setCharacteristicNotification(characteristic, true)) {
            return@withContext false to "setCharacteristicNotification failed"
        }

        val descriptor = characteristic.getDescriptor(CLIENT_CHARACTERISTIC_CONFIG_UUID)
        if (descriptor != null) {
            val deferred = CompletableDeferred<Boolean>()
            bleDescriptorWriteDeferred = deferred
            val writeStarted = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                gatt.writeDescriptor(
                    descriptor,
                    BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
                ) == BluetoothStatusCodes.SUCCESS
            } else {
                @Suppress("DEPRECATION")
                descriptor.value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
                @Suppress("DEPRECATION")
                gatt.writeDescriptor(descriptor)
            }
            if (!writeStarted) {
                bleDescriptorWriteDeferred = null
                return@withContext false to "BLE CCCD write failed to start"
            }
            val descriptorOk = withTimeoutOrNull(5_000L) { deferred.await() } == true
            if (!descriptorOk) {
                return@withContext false to "BLE CCCD write timed out or failed"
            }
        }

        // 等这次读真正回来再返回。GATT 一次只能有一个操作在途：以前读完不等就开始发协议包，
        // 第一包写入常常因为读还在途而起不来（writeCharacteristic 返回失败），SAR 当成发送失败
        // 整个会话重来。需要加密时系统会先弹配对，读要等配对结束才回来，也顺带让协议包等到配对之后。
        // 读失败照旧继续：这一步一直是尽力而为。
        bleServiceProbeCharacteristic?.let { probe ->
            val readDeferred = CompletableDeferred<Boolean>()
            bleReadDeferred = readDeferred
            val readStarted = runCatching {
                @Suppress("DEPRECATION")
                gatt.readCharacteristic(probe)
            }.getOrDefault(false)
            if (readStarted) {
                val ok = withTimeoutOrNull(BLE_SERVICE_PROBE_TIMEOUT_MS) { readDeferred.await() }
                if (ok != true) {
                    webViewLog("Kotlin: BLE service probe read ${if (ok == null) "timed out" else "failed"}")
                }
            }
            if (bleReadDeferred === readDeferred) bleReadDeferred = null
        }

        true to null
    }

    @SuppressLint("MissingPermission")
    suspend fun sendBle(data: ByteArray): Pair<Boolean, String?> = withContext(Dispatchers.IO) {
        bleSendMutex.withLock {
            val gatt = bleGatt ?: return@withLock false to "BLE GATT not connected"
            val characteristic = bleWriteCharacteristic
                ?: return@withLock false to "BLE write characteristic not found"
            val maxLen = getBleMaxSendLen() ?: 20
            if (data.size > maxLen) {
                return@withLock false to "BLE packet too long: ${data.size} > $maxLen"
            }

            characteristic.writeType = BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
            val writeOnce = {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    gatt.writeCharacteristic(
                        characteristic,
                        data,
                        BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
                    ) == BluetoothStatusCodes.SUCCESS
                } else {
                    @Suppress("DEPRECATION")
                    characteristic.value = data
                    @Suppress("DEPRECATION")
                    gatt.writeCharacteristic(characteristic)
                }
            }
            // 写不起来多半是 GATT 上一个操作（读、CCCD、上一包写入）还没回调，系统同一时间只接一个；
            // 等一下再试，而不是立刻报发送失败让 SAR 整个会话重来。
            var writeStarted = writeOnce()
            var attempts = 0
            while (!writeStarted && attempts < BLE_WRITE_BUSY_RETRIES) {
                attempts++
                delay(BLE_WRITE_BUSY_RETRY_DELAY_MS)
                if (bleGatt !== gatt) {
                    return@withLock false to "BLE GATT disconnected"
                }
                writeStarted = writeOnce()
            }
            if (!writeStarted) {
                return@withLock false to "BLE write failed to start"
            }

            delay(BLE_WRITE_DELAY_MS)
            true to null
        }
    }

    @OptIn(ExperimentalCoroutinesApi::class)
    fun send(data: ByteArray): Boolean {
        val actor = sendActor
        val currentSocket = socket
        if (actor == null || actor.isClosedForSend || currentSocket == null || !currentSocket.isConnected) {
            return false
        }
        return actor.trySend(data).isSuccess
    }

    fun startSubscription() {
        if (inStream == null || readThread != null) return
        val reader = Thread {
            val currentThread = Thread.currentThread()
            val buf = ByteArray(1024)
            var disconnectError: IOException? = null
            try {
                while (readThread === currentThread && !currentThread.isInterrupted) {
                    val len = inStream?.read(buf) ?: break
                    if (len <= 0) {
                        disconnectError = IOException("Connection closed by peer")
                        break
                    }
                    val bytes = buf.copyOf(len)
                    if (readThread === currentThread) {
                        uiHandler.post {
                            if (readThread === currentThread) {
                                dataListener?.onDataReceived(bytes)
                            }
                        }
                    }
                }
            } catch (e: IOException) {
                disconnectError = e
            } finally {
                if (readThread === currentThread) {
                    val listener = dataListener
                    val err = disconnectError ?: IOException("Connection closed by peer")
                    uiHandler.post {
                        listener?.onError(err)
                    }
                    disconnect()
                }
            }
        }
        readThread = reader
        reader.start()
    }

    @SuppressLint("MissingPermission")
    fun disconnect() {
        readThread?.interrupt(); readThread = null

        sendActor?.close()
        sendActor = null
        sendScope.coroutineContext.cancelChildren()
        pendingPool.clear()

        try { inStream?.close() } catch (_: Exception) {}
        try { outStream?.close() } catch (_: Exception) {}
        try { socket?.close() } catch (_: Exception) {}
        inStream = null; outStream = null; socket = null; connectedDevice = null
        onConnectedCallback = null
        dataListener = null
    }

    @SuppressLint("MissingPermission")
    fun disconnectBle() {
        bleManualDisconnect = true
        stopBleScan()

        bleConnectDeferred?.complete(false)
        bleConnectDeferred = null
        bleServicesDeferred?.complete(false)
        bleServicesDeferred = null
        bleMtuDeferred?.complete(BLE_DEFAULT_MTU)
        bleMtuDeferred = null
        bleDescriptorWriteDeferred?.complete(false)
        bleDescriptorWriteDeferred = null
        bleReadDeferred?.complete(false)
        bleReadDeferred = null

        runCatching { bleGatt?.disconnect() }
        runCatching { bleGatt?.close() }
        bleGatt = null
        bleConnectedDevice = null
        bleWriteCharacteristic = null
        bleNotifyCharacteristic = null
        bleServiceProbeCharacteristic = null
        bleMtu = BLE_DEFAULT_MTU
        onConnectedCallback = null
        dataListener = null
        bleManualDisconnect = false
    }

    private val scanReceiver = object : BroadcastReceiver() {
        override fun onReceive(ctx: Context?, intent: Intent) {
            if (intent.action == BluetoothDevice.ACTION_FOUND) {
                (intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE) as? BluetoothDevice)
                    ?.takeIf { !scannedDevices.contains(it) }
                    ?.let(scannedDevices::add)
            }
        }
    }
}
