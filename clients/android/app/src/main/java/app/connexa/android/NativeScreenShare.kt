package app.connexa.android

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjection
import android.os.Build
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import org.webrtc.DataChannel
import org.webrtc.DefaultVideoDecoderFactory
import org.webrtc.DefaultVideoEncoderFactory
import org.webrtc.EglBase
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.PeerConnection
import org.webrtc.PeerConnectionFactory
import org.webrtc.RtpTransceiver
import org.webrtc.ScreenCapturerAndroid
import org.webrtc.SdpObserver
import org.webrtc.SessionDescription
import org.webrtc.SurfaceTextureHelper
import org.webrtc.VideoSource
import org.webrtc.VideoTrack

/**
 * Captures the screen with MediaProjection and sends it to each viewer over
 * its own peer connection ("side link"). Android WebView has no
 * getDisplayMedia, so this replaces it. Signaling goes through the page:
 * [emit] delivers offers and ICE candidates to JavaScript, which relays them
 * to viewers as peer messages.
 */
class NativeScreenShare(
    private val context: Context,
    private val emit: (JSONObject) -> Unit,
) {
    private val egl: EglBase = EglBase.create()
    private val factory: PeerConnectionFactory by lazy {
        PeerConnectionFactory.initialize(
            PeerConnectionFactory.InitializationOptions.builder(context).createInitializationOptions(),
        )
        PeerConnectionFactory.builder()
            .setVideoEncoderFactory(DefaultVideoEncoderFactory(egl.eglBaseContext, true, true))
            .setVideoDecoderFactory(DefaultVideoDecoderFactory(egl.eglBaseContext))
            .createPeerConnectionFactory()
    }

    private var capturer: ScreenCapturerAndroid? = null
    private var helper: SurfaceTextureHelper? = null
    private var source: VideoSource? = null
    private var track: VideoTrack? = null
    private var iceServers: List<PeerConnection.IceServer> = emptyList()
    private val links = HashMap<String, PeerConnection>()

    val sharing: Boolean get() = track != null

    fun setIceServers(json: String) {
        iceServers = parseIceServers(json)
    }

    /** Start capturing. Must run after the mediaProjection foreground service started. */
    fun start(permissionResult: Intent, width: Int, height: Int) {
        stop()
        val cap = ScreenCapturerAndroid(permissionResult, object : MediaProjection.Callback() {
            override fun onStop() {
                // The user stopped sharing from the system UI.
                emit(JSONObject().put("type", "stopped"))
            }
        })
        val tex = SurfaceTextureHelper.create("ConnexaScreen", egl.eglBaseContext)
        val src = factory.createVideoSource(true)
        cap.initialize(tex, context, src.capturerObserver)
        cap.startCapture(width, height, FPS)
        capturer = cap
        helper = tex
        source = src
        track = factory.createVideoTrack("screen", src)
    }

    fun offer(peerId: String) {
        val t = track ?: return
        close(peerId)
        val config = PeerConnection.RTCConfiguration(iceServers).apply {
            sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
        }
        val pc = factory.createPeerConnection(config, object : LinkObserver() {
            override fun onIceCandidate(candidate: IceCandidate) {
                emit(
                    JSONObject()
                        .put("type", "ice")
                        .put("peerId", peerId)
                        .put(
                            "candidate",
                            JSONObject()
                                .put("candidate", candidate.sdp)
                                .put("sdpMid", candidate.sdpMid)
                                .put("sdpMLineIndex", candidate.sdpMLineIndex),
                        ),
                )
            }
        }) ?: return
        pc.addTransceiver(
            t,
            RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.SEND_ONLY, listOf(STREAM_ID)),
        )
        links[peerId] = pc
        pc.createOffer(object : Sdp() {
            override fun onCreateSuccess(desc: SessionDescription) {
                pc.setLocalDescription(object : Sdp() {
                    override fun onSetSuccess() {
                        emit(JSONObject().put("type", "offer").put("peerId", peerId).put("sdp", desc.description))
                    }
                }, desc)
            }
        }, MediaConstraints())
    }

    fun answer(peerId: String, sdp: String) {
        links[peerId]?.setRemoteDescription(Sdp(), SessionDescription(SessionDescription.Type.ANSWER, sdp))
    }

    fun candidate(peerId: String, json: String) {
        val c = JSONObject(json)
        links[peerId]?.addIceCandidate(
            IceCandidate(c.optString("sdpMid"), c.optInt("sdpMLineIndex"), c.getString("candidate")),
        )
    }

    fun close(peerId: String) {
        links.remove(peerId)?.dispose()
    }

    fun stop() {
        links.values.forEach { it.dispose() }
        links.clear()
        try {
            capturer?.stopCapture()
        } catch (e: InterruptedException) {
            Log.w(TAG, "stopCapture interrupted", e)
        }
        capturer?.dispose()
        track?.dispose()
        source?.dispose()
        helper?.dispose()
        capturer = null
        track = null
        source = null
        helper = null
    }

    private fun parseIceServers(json: String): List<PeerConnection.IceServer> {
        val out = ArrayList<PeerConnection.IceServer>()
        val arr = JSONArray(json)
        for (i in 0 until arr.length()) {
            val s = arr.getJSONObject(i)
            val urls = s.get("urls").let { u ->
                if (u is JSONArray) (0 until u.length()).map { u.getString(it) } else listOf(u.toString())
            }
            val b = PeerConnection.IceServer.builder(urls)
            if (s.has("username")) b.setUsername(s.getString("username"))
            if (s.has("credential")) b.setPassword(s.getString("credential"))
            out.add(b.createIceServer())
        }
        return out
    }

    /** SDP observer with no-op defaults. */
    private open class Sdp : SdpObserver {
        override fun onCreateSuccess(desc: SessionDescription) {}
        override fun onSetSuccess() {}
        override fun onCreateFailure(error: String?) {
            Log.w(TAG, "SDP create failed: $error")
        }

        override fun onSetFailure(error: String?) {
            Log.w(TAG, "SDP set failed: $error")
        }
    }

    /** Peer connection observer with no-op defaults. */
    private open class LinkObserver : PeerConnection.Observer {
        override fun onSignalingChange(state: PeerConnection.SignalingState) {}
        override fun onIceConnectionChange(state: PeerConnection.IceConnectionState) {}
        override fun onIceConnectionReceivingChange(receiving: Boolean) {}
        override fun onIceGatheringChange(state: PeerConnection.IceGatheringState) {}
        override fun onIceCandidate(candidate: IceCandidate) {}
        override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>) {}
        override fun onAddStream(stream: MediaStream) {}
        override fun onRemoveStream(stream: MediaStream) {}
        override fun onDataChannel(channel: DataChannel) {}
        override fun onRenegotiationNeeded() {}
    }

    companion object {
        const val STREAM_ID = "android-screen"
        private const val TAG = "ConnexaScreen"
        private const val FPS = 15

        /** Capture size: the display scaled so the long side is at most 1280 px. */
        fun captureSize(activity: Activity): Pair<Int, Int> {
            val (w, h) = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                val b = activity.windowManager.currentWindowMetrics.bounds
                b.width() to b.height()
            } else {
                val m = android.util.DisplayMetrics()
                @Suppress("DEPRECATION")
                activity.windowManager.defaultDisplay.getRealMetrics(m)
                m.widthPixels to m.heightPixels
            }
            val scale = minOf(1.0, 1280.0 / maxOf(w, h))
            // Encoders want even dimensions.
            return ((w * scale).toInt() and 1.inv()) to ((h * scale).toInt() and 1.inv())
        }
    }
}
