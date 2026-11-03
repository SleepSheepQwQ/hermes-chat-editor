package com.sleepsheep.hermeschat

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * Self-test suite shared by the in-app "自检" screen and the external
 * test hook (`am start -n com.sleepsheep.hermeschat/.MainActivity --ez selftest true`).
 *
 * The write probe only runs when the bridge is writable and creates then
 * deletes its own throwaway session, so a passing run leaves no residue.
 */
object TestRunner {

    data class Result(val name: String, val ok: Boolean, val detail: String)

    private const val TAG = "SELFTEST"

    fun run(ctx: Context): List<Result> {
        val bridge = Bridge(Prefs.base(ctx), Prefs.token(ctx))
        val out = mutableListOf<Result>()

        fun check(name: String, block: () -> String) {
            try {
                out.add(Result(name, true, block()))
            } catch (t: Throwable) {
                out.add(Result(name, false, "${t.javaClass.simpleName}: ${t.message}"))
            }
        }

        var probeSession: String? = null

        check("bridge /health") {
            val h = bridge.health()
            if (!h.optBoolean("ok", false)) throw IllegalStateException("ok=false")
            "db=${h.optString("db")} writable=${h.optBoolean("writable")} " +
                "hermesRunning=${h.optJSONObject("hermes")?.optBoolean("running")}"
        }

        check("bridge /selftest") {
            val st = bridge.selftest()
            val failed = mutableListOf<String>()
            val checks = st.optJSONArray("checks") ?: JSONArray()
            for (i in 0 until checks.length()) {
                val c = checks.getJSONObject(i)
                if (!c.optBoolean("ok")) failed.add(c.optString("name") + ": " + c.optString("detail"))
            }
            if (failed.isNotEmpty()) throw IllegalStateException(failed.joinToString("; "))
            "${checks.length()} bridge checks passed"
        }

        check("list sessions") {
            val list = bridge.sessions()
            if (list.isEmpty()) throw IllegalStateException("no sessions")
            val filled = list.count { it.messageCount > 0 }
            "${list.size} sessions ($filled with messages), newest=${list.first().id}"
        }

        // Newest sessions can legitimately be empty (imports, probe leftovers),
        // so walk the list until a session actually has rows to read.
        check("read messages") {
            for (s in bridge.sessions()) {
                val msgs = bridge.messages(s.id)
                if (msgs.isNotEmpty()) {
                    return@check "${msgs.size} messages in ${s.id}, " +
                        "first=#${msgs.first().id} ${msgs.first().role}"
                }
            }
            throw IllegalStateException("no session contains messages")
        }

        check("write probe (create→edit→truncate→delete)") {
            val h = bridge.health()
            if (!h.optBoolean("writable")) return@check "skipped: bridge is read-only"
            val created = bridge.createSession("app-selftest")
            val sid = created.optString("id")
            probeSession = sid
            try {
                val a = bridge.insertMessage(sid, "user", "selftest alpha")
                val b = bridge.insertMessage(sid, "assistant", "selftest beta")
                bridge.insertMessage(sid, "user", "selftest gamma", afterId = a.id)
                val ordered = bridge.messages(sid)
                if (ordered.map { it.content } != listOf("selftest alpha", "selftest gamma", "selftest beta")) {
                    throw IllegalStateException("insert-after order wrong: ${ordered.map { it.content }}")
                }
                bridge.updateMessage(b.id, "selftest beta edited", "assistant")
                bridge.moveMessage(b.id, "up")
                val afterMove = bridge.messages(sid)
                if (afterMove.first { it.id == b.id }.ord > afterMove.first { it.content == "selftest gamma" }.ord) {
                    throw IllegalStateException("move up did not reorder")
                }
                val md = bridge.export(sid, "md")
                if (md.length < 20) throw IllegalStateException("export too small")
                bridge.truncate(sid, a.id)
                val left = bridge.messages(sid)
                if (left.size != 1) throw IllegalStateException("truncate left ${left.size} rows")
                "created $sid, 3 inserts, edit, move, export, truncate OK"
            } finally {
                runCatching { bridge.deleteSession(sid) }
            }
        }

        // cross-check the session count after the probe cleaned up
        check("cleanup verified") {
            if (probeSession == null) return@check "skipped: read-only bridge"
            val still = bridge.sessions().any { it.id == probeSession }
            if (still) throw IllegalStateException("probe session still present")
            "probe session removed"
        }

        writeReport(ctx, out)
        out.forEach { Log.i(TAG, (if (it.ok) "PASS " else "FAIL ") + it.name + " :: " + it.detail) }
        return out
    }

    private fun writeReport(ctx: Context, results: List<Result>) {
        val dir = ctx.getExternalFilesDir("selftest") ?: return
        dir.mkdirs()
        val arr = JSONArray()
        results.forEach {
            arr.put(JSONObject().put("name", it.name).put("ok", it.ok).put("detail", it.detail))
        }
        val doc = JSONObject()
            .put("ok", results.all { it.ok })
            .put("at", System.currentTimeMillis())
            .put("base", Prefs.base(ctx))
            .put("results", arr)
        File(dir, "result.json").writeText(doc.toString(2))
    }
}
