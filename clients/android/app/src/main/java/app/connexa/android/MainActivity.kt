package app.connexa.android

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ContentValues
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.util.Base64
import android.webkit.JavascriptInterface
import android.webkit.PermissionRequest
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.widget.Toast
import androidx.webkit.WebViewAssetLoader
import androidx.webkit.WebViewClientCompat
import java.io.File

/**
 * Connexa for Android: the shared web client in a WebView.
 *
 * The client is bundled in assets/www and served from the virtual origin
 * https://appassets.androidplatform.net, which is a secure context, so
 * getUserMedia (camera, microphone) works. WebRTC permission requests from
 * the page are mapped to Android runtime permissions.
 */
class MainActivity : Activity() {

    private lateinit var webView: WebView
    private var pendingPermission: PermissionRequest? = null
    private var fileCallback: ValueCallback<Array<Uri>>? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        WebView.setWebContentsDebuggingEnabled(BuildConfig.DEBUG)

        val assets = WebViewAssetLoader.Builder()
            .addPathHandler("/assets/", WebViewAssetLoader.AssetsPathHandler(this))
            .build()

        webView = WebView(this)
        setContentView(webView)

        webView.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            mediaPlaybackRequiresUserGesture = false
            allowFileAccess = false
            allowContentAccess = false
            // Lets the page reach ws:// development or LAN servers; production uses wss://.
            mixedContentMode = WebSettings.MIXED_CONTENT_COMPATIBILITY_MODE
        }

        webView.webViewClient = object : WebViewClientCompat() {
            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? =
                assets.shouldInterceptRequest(request.url)

            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                if (request.url.host == APP_HOST) return false
                // Anything else (help links, invite links) opens in the browser.
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, request.url))
                } catch (_: ActivityNotFoundException) {
                }
                return true
            }
        }

        webView.webChromeClient = object : WebChromeClient() {
            override fun onPermissionRequest(request: PermissionRequest) {
                runOnUiThread { handlePermissionRequest(request) }
            }

            override fun onPermissionRequestCanceled(request: PermissionRequest) {
                if (pendingPermission == request) pendingPermission = null
            }

            override fun onShowFileChooser(
                view: WebView,
                callback: ValueCallback<Array<Uri>>,
                params: FileChooserParams,
            ): Boolean {
                fileCallback?.onReceiveValue(null)
                fileCallback = callback
                val intent = Intent(Intent.ACTION_GET_CONTENT).apply {
                    addCategory(Intent.CATEGORY_OPENABLE)
                    type = "*/*"
                    putExtra(Intent.EXTRA_ALLOW_MULTIPLE, params.mode == FileChooserParams.MODE_OPEN_MULTIPLE)
                }
                return try {
                    @Suppress("DEPRECATION")
                    startActivityForResult(Intent.createChooser(intent, "Send files"), REQUEST_FILES)
                    true
                } catch (_: ActivityNotFoundException) {
                    fileCallback = null
                    false
                }
            }
        }

        webView.addJavascriptInterface(Bridge(), "ConnexaAndroid")
        if (savedInstanceState == null) {
            webView.loadUrl("https://$APP_HOST/assets/www/index.html")
        } else {
            webView.restoreState(savedInstanceState)
        }
    }

    private fun handlePermissionRequest(request: PermissionRequest) {
        if (request.origin.host != APP_HOST) {
            request.deny()
            return
        }
        val needed = request.resources.mapNotNull {
            when (it) {
                PermissionRequest.RESOURCE_VIDEO_CAPTURE -> Manifest.permission.CAMERA
                PermissionRequest.RESOURCE_AUDIO_CAPTURE -> Manifest.permission.RECORD_AUDIO
                else -> null
            }
        }
        val missing = needed.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (missing.isEmpty()) {
            request.grant(request.resources)
        } else {
            pendingPermission?.deny()
            pendingPermission = request
            requestPermissions(missing.toTypedArray(), REQUEST_MEDIA)
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode != REQUEST_MEDIA) return
        val request = pendingPermission ?: return
        pendingPermission = null
        val granted = request.resources.filter {
            when (it) {
                PermissionRequest.RESOURCE_VIDEO_CAPTURE ->
                    checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
                PermissionRequest.RESOURCE_AUDIO_CAPTURE ->
                    checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
                else -> false
            }
        }
        if (granted.isEmpty()) request.deny() else request.grant(granted.toTypedArray())
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_FILES) return
        val callback = fileCallback ?: return
        fileCallback = null
        if (resultCode != RESULT_OK || data == null) {
            callback.onReceiveValue(null)
            return
        }
        val uris = data.clipData?.let { clip -> (0 until clip.itemCount).map { clip.getItemAt(it).uri } }
            ?: listOfNotNull(data.data)
        callback.onReceiveValue(uris.toTypedArray())
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        webView.saveState(outState)
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (webView.canGoBack()) webView.goBack() else @Suppress("DEPRECATION") super.onBackPressed()
    }

    override fun onDestroy() {
        webView.destroy()
        super.onDestroy()
    }

    /** Exposed to the page as `window.ConnexaAndroid`. Only our bundled page is ever loaded. */
    inner class Bridge {
        @JavascriptInterface
        fun saveFile(name: String, mime: String, base64: String): String {
            val bytes = Base64.decode(base64, Base64.DEFAULT)
            val safeName = sanitize(name)
            val location = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, safeName)
                    put(MediaStore.Downloads.MIME_TYPE, mime.ifBlank { "application/octet-stream" })
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                val uri = contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: throw IllegalStateException("could not create download")
                contentResolver.openOutputStream(uri)!!.use { it.write(bytes) }
                values.clear()
                values.put(MediaStore.Downloads.IS_PENDING, 0)
                contentResolver.update(uri, values, null, null)
                "Downloads/$safeName"
            } else {
                val dir = getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: filesDir
                val file = File(dir, safeName)
                file.writeBytes(bytes)
                file.absolutePath
            }
            runOnUiThread { Toast.makeText(this@MainActivity, "Saved to $location", Toast.LENGTH_SHORT).show() }
            return location
        }
    }

    companion object {
        private const val APP_HOST = "appassets.androidplatform.net"
        private const val REQUEST_MEDIA = 1
        private const val REQUEST_FILES = 2

        fun sanitize(name: String): String {
            val cleaned = name.map { if (it.isISOControl() || "<>:\"/\\|?*".contains(it)) '_' else it }
                .joinToString("")
                .trim()
                .trimStart('.')
                .take(120)
            return cleaned.ifBlank { "file" }
        }
    }
}
