// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreJson.kt

package com.sfourdrinier.unifiedblemanager.rustcore

/**
 * Minimal strict JSON reader/writer for the few values the native module
 * itself must inspect or author: the `session.dispose` envelope outcome, the
 * `restorationIdentity` request/answer and structured rejection messages.
 * Operation arguments and results are never parsed here — they cross
 * verbatim between JS and Rust. Hand-rolled because `org.json` is a stub in
 * JVM unit tests.
 */
internal object RustCoreJson {
  class MalformedJson(message: String) : IllegalArgumentException(message)

  fun parse(text: String): Any? {
    val reader = Reader(text)
    reader.skipWhitespace()
    val value = reader.value()
    reader.skipWhitespace()
    if (!reader.atEnd()) throw MalformedJson("trailing characters at ${reader.index}")
    return value
  }

  /** Serializes maps (insertion order), lists, strings, booleans, integers and null. */
  fun write(value: Any?): String = StringBuilder().also { appendValue(it, value) }.toString()

  private fun appendValue(out: StringBuilder, value: Any?) {
    when (value) {
      null -> out.append("null")
      is String -> appendString(out, value)
      is Boolean -> out.append(value)
      is Int, is Long -> out.append(value)
      is Map<*, *> -> {
        out.append('{')
        var first = true
        for ((key, entry) in value) {
          require(key is String) { "JSON object keys must be strings" }
          if (!first) out.append(',')
          first = false
          appendString(out, key)
          out.append(':')
          appendValue(out, entry)
        }
        out.append('}')
      }
      is List<*> -> {
        out.append('[')
        value.forEachIndexed { index, entry ->
          if (index > 0) out.append(',')
          appendValue(out, entry)
        }
        out.append(']')
      }
      else -> throw IllegalArgumentException("Unsupported JSON value ${value.javaClass.name}")
    }
  }

  private fun appendString(out: StringBuilder, value: String) {
    out.append('"')
    for (character in value) {
      when {
        character == '"' -> out.append("\\\"")
        character == '\\' -> out.append("\\\\")
        character == '\n' -> out.append("\\n")
        character == '\r' -> out.append("\\r")
        character == '\t' -> out.append("\\t")
        character < ' ' -> out.append(String.format("\\u%04x", character.code))
        else -> out.append(character)
      }
    }
    out.append('"')
  }

  private class Reader(private val text: String) {
    var index = 0

    fun atEnd(): Boolean = index >= text.length

    fun skipWhitespace() {
      while (!atEnd() && text[index] in " \t\r\n") index++
    }

    fun value(): Any? {
      if (atEnd()) throw MalformedJson("unexpected end")
      return when (text[index]) {
        '{' -> objectValue()
        '[' -> arrayValue()
        '"' -> stringValue()
        't' -> literal("true", true)
        'f' -> literal("false", false)
        'n' -> literal("null", null)
        else -> numberValue()
      }
    }

    private fun literal(word: String, result: Any?): Any? {
      if (!text.startsWith(word, index)) throw MalformedJson("bad literal at $index")
      index += word.length
      return result
    }

    private fun objectValue(): Map<String, Any?> {
      val result = LinkedHashMap<String, Any?>()
      index++
      skipWhitespace()
      if (!atEnd() && text[index] == '}') {
        index++
        return result
      }
      while (true) {
        skipWhitespace()
        if (atEnd() || text[index] != '"') throw MalformedJson("object key expected at $index")
        val key = stringValue()
        if (result.containsKey(key)) throw MalformedJson("duplicate key $key")
        skipWhitespace()
        expect(':')
        skipWhitespace()
        result[key] = value()
        skipWhitespace()
        if (atEnd()) throw MalformedJson("unterminated object")
        if (text[index] == ',') {
          index++
          continue
        }
        expect('}')
        return result
      }
    }

    private fun arrayValue(): List<Any?> {
      val result = ArrayList<Any?>()
      index++
      skipWhitespace()
      if (!atEnd() && text[index] == ']') {
        index++
        return result
      }
      while (true) {
        skipWhitespace()
        result.add(value())
        skipWhitespace()
        if (atEnd()) throw MalformedJson("unterminated array")
        if (text[index] == ',') {
          index++
          continue
        }
        expect(']')
        return result
      }
    }

    private fun stringValue(): String {
      expect('"')
      val out = StringBuilder()
      while (true) {
        if (atEnd()) throw MalformedJson("unterminated string")
        val character = text[index++]
        when {
          character == '"' -> return out.toString()
          character == '\\' -> {
            if (atEnd()) throw MalformedJson("unterminated escape")
            when (val escaped = text[index++]) {
              '"', '\\', '/' -> out.append(escaped)
              'b' -> out.append('\b')
              'f' -> out.append('')
              'n' -> out.append('\n')
              'r' -> out.append('\r')
              't' -> out.append('\t')
              'u' -> {
                if (index + 4 > text.length) throw MalformedJson("short unicode escape")
                out.append(text.substring(index, index + 4).toInt(16).toChar())
                index += 4
              }
              else -> throw MalformedJson("bad escape \\$escaped")
            }
          }
          character < ' ' -> throw MalformedJson("control character in string")
          else -> out.append(character)
        }
      }
    }

    private fun numberValue(): Long {
      val start = index
      if (!atEnd() && text[index] == '-') index++
      while (!atEnd() && text[index].isDigit()) index++
      if (index == start || (index == start + 1 && text[start] == '-')) {
        throw MalformedJson("value expected at $start")
      }
      if (!atEnd() && text[index] in ".eE") throw MalformedJson("only integers are accepted at $start")
      return text.substring(start, index).toLongOrNull() ?: throw MalformedJson("integer out of range at $start")
    }

    private fun expect(character: Char) {
      if (atEnd() || text[index] != character) throw MalformedJson("'$character' expected at $index")
      index++
    }
  }
}
