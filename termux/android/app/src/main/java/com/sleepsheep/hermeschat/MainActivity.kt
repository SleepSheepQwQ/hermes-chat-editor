package com.sleepsheep.hermeschat

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.ViewGroup
import android.widget.BaseAdapter
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ListView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

class MainActivity : Activity() {

    private lateinit var statusView: TextView
    private lateinit var listView: ListView
    private lateinit var searchInput: EditText
    private var shown: List<Session> = emptyList()
    private var showHidden = false

    private val bridge: Bridge
        get() = Bridge(Prefs.base(this), Prefs.token(this))

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildUi())
        if (intent?.getBooleanExtra("selftest", false) == true) {
            startActivity(Intent(this, SelfTestActivity::class.java))
        }
        load()
    }

    private fun buildUi(): LinearLayout {
        val root = vStack(0)

        val header = hStack().apply { setPadding(dp(16), dp(12), dp(16), dp(4)) }
        header.addView(
            label("Hermes Chat · Termux", 18f, bold = true)
                .apply { layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f) }
        )
        header.addView(action("设置") { showSettings() })
        root.addView(header)

        statusView = label("连接中…", 12f, color = 0xFF6B7280.toInt()).apply {
            setPadding(dp(16), 0, dp(16), dp(6))
        }
        root.addView(statusView)

        val searchRow = hStack().apply { setPadding(dp(16), dp(4), dp(16), dp(4)) }
        searchInput = input("搜索标题 / 会话 id").apply {
            setSingleLine(true)
            inputType = InputType.TYPE_CLASS_TEXT
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        searchRow.addView(searchInput)
        searchRow.addView(action("搜索") { load() })
        searchRow.addView(action("刷新") { load() })
        root.addView(searchRow)

        val tools = hStack().apply { setPadding(dp(16), dp(0), dp(16), dp(4)) }
        tools.addView(action("自检") { startActivity(Intent(this, SelfTestActivity::class.java)) })
        tools.addView(CheckBox(this).apply {
            text = "含隐藏"
            textSize = 12f
            setOnCheckedChangeListener { _, checked -> showHidden = checked; load() }
        })
        root.addView(tools)

        listView = ListView(this).apply {
            dividerHeight = dp(1)
            divider = android.graphics.drawable.ColorDrawable(0xFFEEEEEE.toInt())
            setOnItemClickListener { _, _, pos, _ -> if (pos < shown.size) open(shown[pos]) }
            setOnItemLongClickListener { _, _, pos, _ ->
                if (pos < shown.size) { sessionMenu(shown[pos]); true } else false
            }
        }
        root.addView(listView, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))

        root.addView(
            label("读默认为只读；编辑需要 bridge 以 --writable 启动", 11f, color = 0xFF9CA3AF.toInt())
                .apply { setPadding(dp(16), dp(6), dp(16), dp(10)) }
        )
        listView.adapter = SessionAdapter()
        return root
    }

    private fun load() {
        statusView.text = "连接中…"
        val q = searchInput.text.toString()
        runBg({ bridge.health() to bridge.sessions(q, showHidden) }) { pair, err ->
            if (err != null || pair == null) {
                statusView.setTextColor(0xFFB91C1C.toInt())
                statusView.text = "✗ ${err?.message ?: "连接失败"}"
                shown = emptyList()
                (listView.adapter as? SessionAdapter)?.notifyDataSetChanged()
                return@runBg
            }
            val health = pair.first
            val sessions = pair.second
            val writable = health.optBoolean("writable", false)
            val hermes = health.optJSONObject("hermes")?.optBoolean("running", false) ?: false
            statusView.setTextColor(0xFF166534.toInt())
            statusView.text = buildString {
                append("✓ 已连接 · ${sessions.size} 个会话 · ")
                append(if (writable) "可写" else "只读")
                if (hermes) append(" · Hermes 运行中")
            }
            shown = sessions
            (listView.adapter as? SessionAdapter)?.notifyDataSetChanged()
        }
    }

    private fun open(s: Session) {
        startActivity(
            Intent(this, MessagesActivity::class.java)
                .putExtra("session_id", s.id)
                .putExtra("title", s.title)
        )
    }

    private fun sessionMenu(s: Session) {
        AlertDialog.Builder(this)
            .setTitle(s.title)
            .setItems(arrayOf("会话设置", "导出 JSONL", "删除会话")) { _, which ->
                when (which) {
                    0 -> sessionSettings(s)
                    1 -> export(s)
                    2 -> confirmDelete(s)
                }
            }
            .show()
    }

    private fun sessionSettings(s: Session) {
        val box = vStack(10)
        val title = input("标题", s.title)
        val source = input("source（cli / webui / oneshot …）", s.source)
        val pinned = CheckBox(this).apply { text = "置顶"; isChecked = s.pinned }
        val archived = CheckBox(this).apply { text = "归档"; isChecked = s.archived }
        box.addView(title); box.addView(source); box.addView(pinned); box.addView(archived)

        AlertDialog.Builder(this)
            .setTitle("会话设置")
            .setView(box)
            .setPositiveButton("保存") { _, _ ->
                val body = JSONObject()
                    .put("title", title.text.toString())
                    .put("source", source.text.toString())
                    .put("pinned", if (pinned.isChecked) 1 else 0)
                    .put("archived", if (archived.isChecked) 1 else 0)
                runBg({ bridge.updateSession(s.id, body) }) { _, err ->
                    toast(if (err == null) "已保存" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun export(s: Session) {
        runBg({ bridge.export(s.id, "jsonl") }) { text, err ->
            if (err != null || text == null) { toast("导出失败：${err?.message}"); return@runBg }
            val dir = getExternalFilesDir("export")
            if (dir == null) { toast("无外部目录"); return@runBg }
            dir.mkdirs()
            val f = java.io.File(dir, "${s.id}.jsonl")
            f.writeText(text)
            toast("已导出：${f.absolutePath}")
        }
    }

    private fun confirmDelete(s: Session) {
        AlertDialog.Builder(this)
            .setTitle("删除会话")
            .setMessage("将删除 ${s.id} 及其全部消息，并先写备份。继续？")
            .setPositiveButton("删除") { _, _ ->
                runBg({ bridge.deleteSession(s.id) }) { _, err ->
                    toast(if (err == null) "已删除" else "失败：${err.message}")
                    load()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun showSettings() {
        val box = vStack(10)
        val url = input("bridge 地址", Prefs.base(this))
        val token = input("token（X-Bridge-Token）", Prefs.token(this))
        box.addView(label("Bridge 地址", 12f, color = 0xFF6B7280.toInt()))
        box.addView(url)
        box.addView(label("Token（~/.hermes/chat_bridge_token）", 12f, color = 0xFF6B7280.toInt()))
        box.addView(token)

        AlertDialog.Builder(this)
            .setTitle("连接设置")
            .setView(box)
            .setPositiveButton("保存并重连") { _, _ ->
                Prefs.save(this, url.text.toString(), token.text.toString())
                load()
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun toast(msg: String) = Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

    private inner class SessionAdapter : BaseAdapter() {
        override fun getCount() = shown.size
        override fun getItem(position: Int) = shown[position]
        override fun getItemId(position: Int) = position.toLong()

        override fun getView(position: Int, convertView: android.view.View?, parent: ViewGroup?): android.view.View {
            val s = shown[position]
            val ctx = this@MainActivity
            val row = LinearLayout(ctx).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(16), dp(10), dp(16), dp(10))
            }
            row.addView(ctx.label(
                (if (s.pinned) "★ " else "") + s.title,
                15f, bold = true,
                color = if (s.archived) 0xFF9CA3AF.toInt() else 0xFF111827.toInt()
            ))
            val when_ = if (s.lastActivity > 0)
                SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()).format(Date((s.lastActivity * 1000).toLong()))
            else "—"
            row.addView(ctx.label("${s.source} · ${s.messageCount} 条 · $when_ · ${s.id}", 11f, color = 0xFF6B7280.toInt()))
            return row
        }
    }
}
