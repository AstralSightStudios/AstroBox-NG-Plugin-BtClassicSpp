package com.astralsight.astrobox.plugin.btclassic_spp

import app.tauri.annotation.InvokeArg
import app.tauri.plugin.Channel

class RustTypes {
    @InvokeArg
    class SPPDevice {
        var name: String = "";
        var address: String = "";
    }

    @InvokeArg
    class AddressArg {
        lateinit var addr: String
    }

    @InvokeArg
    class AddressSendPayload {
        lateinit var addr: String
        var b64data: String = "";
    }

    /** Channel is deliberately a direct field in the address payload. */
    @InvokeArg
    class AddressChannelArg {
        lateinit var addr: String
        lateinit var channel: Channel
    }
}
