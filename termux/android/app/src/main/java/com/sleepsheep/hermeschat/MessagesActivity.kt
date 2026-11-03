package com.sleepsheep.hermeschat

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.os.Bundle
import android.view.ViewGroup
import android.widget.ArrayAdapter
import android.widget.BaseAdapter
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ListView
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

class MessagesActivity : Activity() {

    private lateinit var sessionId: String
    private lateinit var listView: ListView
    private lateinit var statusView: TextView
    private var messages: List<Message> = emptyList()
    private val roles = listOf("user", "assistant", "system", "tool")

    private val bridge: Bridge
        get() = Bridge(Prefs.base(this), Prefs.token(this))

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        sessionId = intent?.getStringExtra("session_id") ?: run { finish(); return }
        val title = intent?.getStringExtra("title") ?: sessionId
        setContentView(buildUi(title))
        load()
    }

    private fun buildUi(title: String): LinearLayout {
        val root = vStack(0)

        val header = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = android.view.Gravity.CENTER_VERTICAL
            setPadding(dp(12), dp(10), dp(12), dp(4))
        }
        header.addView(action("‹ 返回") { finish() })
        header.addView(
            label(title, 15f, bold = true)
                .apply { layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f) }
        )
        header.addView(action("设置") { sessionSettings() })
        root.addView(header)

        statusView = label("加载中…", 11f, color = 0xFF6B7280.toInt()).apply {
            setPadding(dp(14), 0, dp(14), dp(4))
        }
        root.addView(statusView)

        listView = ListView(this).apply {
            dividerHeight = dp(1)
            divider = android.graphics.drawable.ColorDrawable(0xFFEEEEEE.toInt())
            setOnItemClickListener { _, _, pos, _ -> if (pos < messages.size) edit(messages[pos]) }
            setOnItemLongClickListener { _, _, pos, _ ->
                if (pos < messages.size) { messageMenu(messages[pos]); true } else false
            }
        }
        root.addView(listView, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))

        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(8), dp(4), dp(8), dp(8))
        }
        bar.addView(action("刷新") { load() })
        bar.addView(action("＋插入") { insertDialog(null) })
        bar.addView(action("导出") { exportDialog() })
        root.addView(bar)
        return root
    }

    private fun load() {
        statusView.text = "加载中…"
        runBg({ bridge.messages(sessionId) }) { list, err ->
            if (err != null || list == null) {
                statusView.setTextColor(0xFFB91C1C.toInt())
                statusView.text = "✗ ${err?.message}"
                return@runBg
            }
            messages = list
            statusView.setTextColor(0xFF6B7280.toInt())
            statusView.text = "${list.size} 条消息 · ${sessionId}"
            (listView.adapter as? MessageAdapter)?.notifyDataSetChanged()
        }
    }

    private fun edit(m: Message) {
        val box = vStack(8)
        val roleSpinner = Spinner(this).apply {
            adapter = ArrayAdapter(this@MessagesActivity, android.R.layout.simple_spinner_dropdown_item, roles)
            setSelection(roles.indexOf(m.role).coerceAtLeast(0))
        }
        val content = EditText(this).apply {
            setText(m.content)
            minLines = 6
            textSize = 14f
        }
        box.addView(roleSpinner)
        box.addView(content)

        AlertDialog.Builder(this)
            .setTitle("#${m.id}")
            .setView(box)
            .setPositiveButton("保存") { _, _ ->
                val newRole = roles[roleSpinner.selectedItemPosition]
                runBg({ bridge.updateMessage(m.id, content.text.toString(), newRole) }) { _, err ->
                    toast(if (err == null) "已保存" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun messageMenu(m: Message) {
        val items = arrayOf("编辑", "在此后插入", "上移", "下移", "删除", "截断到此")
        AlertDialog.Builder(this)
            .setTitle("#${m.id} ${m.role}")
            .setItems(items) { _, which ->
                when (which) {
                    0 -> edit(m)
                    1 -> insertDialog(m.id)
                    2 -> move(m, "up")
                    3 -> move(m, "down")
                    4 -> confirmDelete(m)
                    5 -> confirmTruncate(m)
                }
            }
            .show()
    }

    private fun insertDialog(afterId: Long?) {
        val box = vStack(8)
        val roleSpinner = Spinner(this).apply {
            adapter = ArrayAdapter(this@MessagesActivity, android.R.layout.simple_spinner_dropdown_item, roles)
        }
        val content = EditText(this).apply {
            hint = if (afterId == null) "追加到会话末尾" else "插入到 #$afterId 之后"
            minLines = 5
            textSize = 14f
        }
        box.addView(roleSpinner)
        box.addView(content)

        AlertDialog.Builder(this)
            .setTitle("插入消息")
            .setView(box)
            .setPositiveButton("插入") { _, _ ->
                val role = roles[roleSpinner.selectedItemPosition]
                val text = content.text.toString()
                if (text.isEmpty()) { toast("内容为空"); return@setPositiveButton }
                runBg({ bridge.insertMessage(sessionId, role, text, afterId) }) { _, err ->
                    toast(if (err == null) "已插入" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun move(m: Message, direction: String) {
        runBg({ bridge.moveMessage(m.id, direction) }) { _, err ->
            if (err != null) toast("失败：${err.message}")
            load()
        }
    }

    private fun confirmDelete(m: Message) {
        AlertDialog.Builder(this)
            .setTitle("删除 #${m.id}")
            .setMessage("消息删除后不可恢复（会先写备份）。继续？")
            .setPositiveButton("删除") { _, _ ->
                runBg({ bridge.deleteMessage(m.id) }) { _, err ->
                    toast(if (err == null) "已删除" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun confirmTruncate(m: Message) {
        AlertDialog.Builder(this)
            .setTitle("截断")
            .setMessage("删除 #${m.id} 之后的全部消息？")
            .setPositiveButton("截断") { _, _ ->
                runBg({ bridge.truncate(sessionId, m.id) }) { _, err ->
                    toast(if (err == null) "已截断" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun exportDialog() {
        val formats = arrayOf("jsonl", "md", "html")
        AlertDialog.Builder(this)
            .setTitle("导出会话")
            .setItems(formats) { _, which ->
                val fmt = formats[which]
                runBg({ bridge.export(sessionId, fmt) }) { text, err ->
                    if (err != null || text == null) { toast("失败：${err?.message}"); return@runBg }
                    val dir = getExternalFilesDir("export")
                    if (dir == null) { toast("无外部目录"); return@runBg }
                    dir.mkdirs()
                    val f = java.io.File(dir, "$sessionId.$fmt")
                    f.writeText(text)
                    toast("已导出：${f.absolutePath}")
                }
            }
            .show()
    }

    private fun sessionSettings() {
        val box = vStack(8)
        val idInput = input("会话 id", sessionId).apply { isEnabled = false }
        val title = input("标题", "")
        val source = input("source", "")
        val prompt = input("系统提示词（留空不改）").apply { minLines = 3 }
        box.addView(idInput); box.addView(title); box.addView(source); box.addView(prompt)

        AlertDialog.Builder(this)
            .setTitle("会话设置")
            .setView(box)
            .setPositiveButton("保存") { _, _ ->
                val body = JSONObject()
                if (title.text.isNotBlank()) body.put("title", title.text.toString())
                if (source.text.isNotBlank()) body.put("source", source.text.toString())
                if (prompt.text.isNotBlank()) body.put("system_prompt", prompt.text.toString())
                if (body.length() == 0) { toast("没有改动"); return@setPositiveButton }
                runBg({ bridge.updateSession(sessionId, body) }) { _, err ->
                    toast(if (err == null) "已保存" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun toast(msg: String) = Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

    private inner class MessageAdapter : BaseAdapter() {
        override fun getCount() = messages.size
        override fun getItem(position: Int) = messages[position]
        override fun getItemId(position: Int) = messages[position].id

        override fun getView(position: Int, convertView: android.view.View?, parent: ViewGroup?): android.view.View {
            val m = messages[position]
            val ctx = this@MessagesActivity
            val row = LinearLayout(ctx).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(14), dp(8), dp(14), dp(8))
            }
            val roleColor = when (m.role) {
                "user" -> 0xFF1F6FEB.toInt()
                "assistant" -> 0xFF15803D.toInt()
                "system" -> 0xFFB45309.toInt()
                else -> 0xFF6D28D9.toInt()
            }
            val head = LinearLayout(ctx).apply { orientation = LinearLayout.HORIZONTAL }
            head.addView(ctx.label(m.role, 12f, bold = true, color = roleColor))
            head.addView(ctx.label("  #${m.id} · ord ${m.ord}", 10f, color = 0xFF9CA3AF.toInt()))
            row.addView(head)

            val body = m.content.trim().ifEmpty {
                if (m.hasToolCalls) "工具调用 → ${m.toolName ?: "tool"}" else "（空）"
            }
            row.addView(ctx.label(body.take(600), 13f, color = 0xFF1F2937.toInt()))
            return row
        }
    }
}
