package com.sleepsheep.hermeschat

import android.app.Activity
import android.content.Context
import android.graphics.Typeface
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView

fun Context.dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

fun Context.vStack(pad: Int = 12): LinearLayout = LinearLayout(this).apply {
    orientation = LinearLayout.VERTICAL
    setPadding(dp(pad), dp(pad), dp(pad), dp(pad))
}

fun Context.hStack(): LinearLayout = LinearLayout(this).apply {
    orientation = LinearLayout.HORIZONTAL
    gravity = android.view.Gravity.CENTER_VERTICAL
}

fun Context.label(
    text: String,
    size: Float = 14f,
    bold: Boolean = false,
    color: Int = 0xFF111827.toInt(),
): TextView = TextView(this).apply {
    this.text = text
    textSize = size
    setTextColor(color)
    if (bold) setTypeface(typeface, Typeface.BOLD)
}

fun Context.action(text: String, onClick: () -> Unit): Button = Button(this).apply {
    this.text = text
    textSize = 13f
    isAllCaps = false
    setOnClickListener { onClick() }
}

fun Context.input(hintText: String, value: String = ""): EditText = EditText(this).apply {
    hint = hintText
    setText(value)
    textSize = 14f
}

/** Run blocking work off the UI thread, deliver both value and error. */
fun <T> Activity.runBg(work: () -> T, done: (T?, Throwable?) -> Unit) {
    Thread {
        var value: T? = null
        var error: Throwable? = null
        try {
            value = work()
        } catch (t: Throwable) {
            error = t
        }
        runOnUiThread { done(value, error) }
    }.start()
}
