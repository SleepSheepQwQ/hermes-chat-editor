package com.sleepsheep.hermeschat

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

/** One Hermes session row. */
data class Session(
    val id: String,
    val title: String,
    val source: String,
    val messageCount: Int,
    val lastActivity: Double,
    val pinned: Boolean,
    val archived: Boolean,
)

/** One Hermes message row. */
data class Message(
    val id: Long,
    val role: String,
    val content: String,
    val ord: Long,
    val timestamp: Double,
    val toolName: String?,
    val hasToolCalls: Boolean,
)

class BridgeException(val code: Int, message: String) : Exception(message)

/** org.json turns JSON null into the literal "null"; keep that out of the UI. */
fun JSONObject.text(key: String): String = if (isNull(key)) "" else optString(key, "")

/**
 * Thin HTTP client for the Termux bridge (termux/bridge/hermes_chat_bridge.py).
 * No third-party deps: HttpURLConnection + org.json, both in the framework.
 */
class Bridge(baseUrl: String, token: String) {
    private val base = baseUrl.trimEnd('/')
    private val token = token.trim()

    private fun request(method: String, path: String, body: String? = null): String {
        val conn = URL(base + path).openConnection() as HttpURLConnection
        try {
            conn.requestMethod = method
            conn.connectTimeout = 5_000
            conn.readTimeout = 20_000
            conn.setRequestProperty("Accept", "application/json")
            if (token.isNotEmpty()) conn.setRequestProperty("X-Bridge-Token", token)
            if (body != null) {
                conn.doOutput = true
                conn.setRequestProperty("Content-Type", "application/json; charset=utf-8")
                conn.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
            }
            val code = conn.responseCode
            val stream = if (code in 200..299) conn.inputStream else conn.errorStream
            val text = runCatching {
                stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() }
            }.getOrNull().orEmpty()
            if (code !in 200..299) {
                val msg = runCatching { JSONObject(text).optString("error") }
                    .getOrDefault(text).ifBlank { "HTTP $code" }
                throw BridgeException(code, msg)
            }
            return text
        } finally {
            conn.disconnect()
        }
    }

    private fun get(path: String) = request("GET", path)
    private fun post(path: String, body: JSONObject) = request("POST", path, body.toString())
    private fun patch(path: String, body: JSONObject) = request("PATCH", path, body.toString())
    private fun delete(path: String) = request("DELETE", path)

    fun health(): JSONObject = JSONObject(get("/health"))
    fun selftest(): JSONObject = JSONObject(get("/selftest"))

    fun sessions(query: String = "", includeHidden: Boolean = false): List<Session> {
        val qs = buildString {
            append("/sessions")
            if (query.isNotBlank()) { append("?q=").append(java.net.URLEncoder.encode(query, "UTF-8")) }
            if (includeHidden) { append(if (query.isBlank()) "?" else "&").append("hidden=1") }
        }
        val arr = JSONArray(get(qs))
        return (0 until arr.length()).map { i ->
            val o = arr.getJSONObject(i)
            Session(
                id = o.optString("id"),
                title = o.text("title").ifBlank { o.optString("id") },
                source = o.text("source"),
                messageCount = o.optInt("message_count"),
                lastActivity = o.optDouble("last_activity_at", 0.0),
                pinned = o.optInt("pinned") == 1,
                archived = o.optInt("archived") == 1,
            )
        }
    }

    fun messages(sessionId: String): List<Message> {
        val arr = JSONArray(get("/sessions/${enc(sessionId)}/messages"))
        return (0 until arr.length()).map { i ->
            val o = arr.getJSONObject(i)
            Message(
                id = o.optLong("id"),
                role = o.text("role"),
                content = o.text("content"),
                ord = o.optLong("ord"),
                timestamp = o.optDouble("timestamp", 0.0),
                toolName = o.text("tool_name").ifBlank { null },
                hasToolCalls = o.text("tool_calls").isNotBlank(),
            )
        }
    }

    fun updateMessage(id: Long, content: String?, role: String?, reasoning: String? = null): Message {
        val body = JSONObject()
        content?.let { body.put("content", it) }
        role?.let { body.put("role", it) }
        reasoning?.let { body.put("reasoning", it) }
        val o = JSONObject(patch("/messages/$id", body))
        return Message(
            id = o.optLong("id"),
            role = o.text("role"),
            content = o.text("content"),
            ord = o.optLong("ord"),
            timestamp = o.optDouble("timestamp", 0.0),
            toolName = o.text("tool_name").ifBlank { null },
            hasToolCalls = o.text("tool_calls").isNotBlank(),
        )
    }

    fun insertMessage(sessionId: String, role: String, content: String, afterId: Long? = null): Message {
        val body = JSONObject().put("role", role).put("content", content)
        if (afterId != null) body.put("after_id", afterId)
        val o = JSONObject(post("/sessions/${enc(sessionId)}/messages", body))
        return Message(
            id = o.optLong("id"),
            role = o.text("role"),
            content = o.text("content"),
            ord = o.optLong("ord"),
            timestamp = o.optDouble("timestamp", 0.0),
            toolName = o.text("tool_name").ifBlank { null },
            hasToolCalls = o.text("tool_calls").isNotBlank(),
        )
    }

    fun deleteMessage(id: Long) = delete("/messages/$id")
    fun moveMessage(id: Long, direction: String) = post("/messages/$id/move", JSONObject().put("direction", direction))
    fun truncate(sessionId: String, afterId: Long) =
        post("/sessions/${enc(sessionId)}/truncate", JSONObject().put("after_id", afterId))

    fun updateSession(sessionId: String, body: JSONObject) = patch("/sessions/${enc(sessionId)}", body)

    fun export(sessionId: String, format: String): String =
        get("/sessions/${enc(sessionId)}/export?format=$format")

    fun createSession(title: String, source: String = "cli"): JSONObject =
        JSONObject(post("/sessions", JSONObject().put("title", title).put("source", source)))

    fun deleteSession(sessionId: String) = delete("/sessions/${enc(sessionId)}")

    companion object {
        private fun enc(s: String): String = java.net.URLEncoder.encode(s, "UTF-8")
    }
}

/** Persisted connection settings. */
object Prefs {
    private const val FILE = "bridge"
    private const val KEY_URL = "base_url"
    private const val KEY_TOKEN = "token"
    const val DEFAULT_URL = "http://127.0.0.1:8765/api/v1"

    private fun sp(ctx: Context) = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    fun base(ctx: Context): String = sp(ctx).getString(KEY_URL, DEFAULT_URL) ?: DEFAULT_URL
    fun token(ctx: Context): String = sp(ctx).getString(KEY_TOKEN, "") ?: ""

    fun save(ctx: Context, base: String, token: String) {
        sp(ctx).edit().putString(KEY_URL, base.trim().ifBlank { DEFAULT_URL })
            .putString(KEY_TOKEN, token.trim()).apply()
    }
}
