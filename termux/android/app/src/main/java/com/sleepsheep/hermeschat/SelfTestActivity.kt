package com.sleepsheep.hermeschat

import android.app.Activity
import android.os.Bundle
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.ListView
import android.widget.TextView

/**
 * Runs the bridge/UI round-trip suite and shows PASS/FAIL.
 *
 * External hook (shell / rish / adb):
 *   am start -n com.sleepsheep.hermeschat/.MainActivity --ez selftest true
 * Report is written to /sdcard/Android/data/com.sleepsheep.hermeschat/files/selftest/result.json
 */
class SelfTestActivity : Activity() {

    private lateinit var status: TextView
    private lateinit var list: ListView
    private var results: List<TestRunner.Result> = emptyList()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildUi())
        run()
    }

    private fun buildUi(): LinearLayout {
        val root = vStack(0)
        val header = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = android.view.Gravity.CENTER_VERTICAL
            setPadding(dp(12), dp(10), dp(12), dp(4))
        }
        header.addView(action("‹ 返回") { finish() })
        header.addView(
            label("自检", 16f, bold = true)
                .apply { layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f) }
        )
        header.addView(action("重跑") { run() })
        root.addView(header)

        status = label("运行中…", 12f, color = 0xFF6B7280.toInt()).apply {
            setPadding(dp(14), 0, dp(14), dp(6))
        }
        root.addView(status)

        list = ListView(this)
        root.addView(list, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        root.addView(
            label("报告：files/selftest/result.json · logcat 标签 SELFTEST", 10f, color = 0xFF9CA3AF.toInt())
                .apply { setPadding(dp(14), dp(4), dp(14), dp(10)) }
        )
        return root
    }

    private fun run() {
        status.text = "运行中…"
        runBg({ TestRunner.run(this) }) { res, err ->
            if (err != null || res == null) {
                status.setTextColor(0xFFB91C1C.toInt())
                status.text = "✗ ${err?.message}"
                return@runBg
            }
            results = res
            val passed = res.count { it.ok }
            status.setTextColor(if (passed == res.size) 0xFF166534.toInt() else 0xFFB91C1C.toInt())
            status.text = "$passed/${res.size} 通过"
            list.adapter = ResultAdapter()
        }
    }

    private inner class ResultAdapter : android.widget.BaseAdapter() {
        override fun getCount() = results.size
        override fun getItem(position: Int) = results[position]
        override fun getItemId(position: Int) = position.toLong()

        override fun getView(position: Int, convertView: android.view.View?, parent: ViewGroup?): android.view.View {
            val r = results[position]
            val ctx = this@SelfTestActivity
            val row = LinearLayout(ctx).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(14), dp(8), dp(14), dp(8))
            }
            row.addView(ctx.label(
                (if (r.ok) "PASS  " else "FAIL  ") + r.name, 13f, bold = true,
                color = if (r.ok) 0xFF166534.toInt() else 0xFFB91C1C.toInt()
            ))
            row.addView(ctx.label(r.detail, 11f, color = 0xFF4B5563.toInt()))
            return row
        }
    }
}
