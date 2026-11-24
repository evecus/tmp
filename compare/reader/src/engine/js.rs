//! QuickJS 求值：执行 jsLib + @js: 表达式，注入 Legado 兼容的绑定变量。
//!
//! 变量体系：JS 层（java.put / source.setVariable）与规则层（@put）共用一张
//! `VarStore = Arc<Mutex<HashMap<String,String>>>`，因此 JS 写入的变量能被
//! 规则层 @get 读到，并在业务链路结束时由 BookService 统一落库。

use crate::cookie::CookieStore;
use crate::error::{AppError, AppResult};
use crate::fetcher::FetchResponse;
use rquickjs::{Context, Function, Object, Runtime, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// 官方 Legado 内置 CryptoJS 源码（64KB，自包含：MD5/SHA/AES/Hmac/Hex/Base64）。
/// 与上游 `assets/scripts/cryptojs.min.js` 完全一致，确保签名型书源语义一致。
const CRYPTO_JS_SOURCE: &str = include_str!("../assets/cryptojs.min.js");

/// 共享变量表：java.put / source.setVariable / 规则 @put 共用
pub type VarStore = Arc<Mutex<HashMap<String, String>>>;

pub fn new_var_store() -> VarStore { Arc::new(Mutex::new(HashMap::new())) }

/// JS 层网络回调：返回完整响应（status/headers/body/url），timeout 为毫秒（0 = 用默认）。
pub type JsFetchFn = Arc<dyn Fn(&str, &str, u64) -> Result<FetchResponse, String> + Send + Sync>;

pub struct JsContext {
    pub js_lib: String,
    pub result: String,
    pub base_url: String,
    pub source_url: String,
    pub source_name: String,
    pub book_name: String,
    pub chapter_title: String,
    /// 共享变量（与规则层 RuleContext 同一张表）
    pub variables: VarStore,
    pub fetch: Option<JsFetchFn>,
    /// jsUrl 专用：搜索关键字、页码
    pub key: String,
    pub page: i32,
    /// 共享 Cookie jar（java.getCookie / java.ajax 复用）
    pub cookies: CookieStore,
    /// 登录信息（getLoginInfoMap 返回）
    pub login_info: VarStore,
}

impl Default for JsContext {
    fn default() -> Self {
        Self {
            js_lib: String::new(), result: String::new(), base_url: String::new(),
            source_url: String::new(), source_name: String::new(), book_name: String::new(),
            chapter_title: String::new(), variables: new_var_store(), fetch: None,
            key: String::new(), page: 1, cookies: CookieStore::new(),
            login_info: new_var_store(),
        }
    }
}

/// 把 JS Value 转成字符串（rquickjs 0.14 API）
fn js_value_to_string(v: &Value<'_>) -> String {
    if let Some(s) = v.as_string() {
        return s.to_string().unwrap_or_default();
    }
    if let Some(b) = v.as_bool() { return b.to_string(); }
    if let Some(i) = v.as_int() { return i.to_string(); }
    if let Some(n) = v.as_number() {
        return if n.fract() == 0.0 { format!("{:.0}", n) } else { n.to_string() };
    }
    if v.is_null() || v.is_undefined() { return String::new(); }
    // 对象/数组需调用方用 Ctx::json_stringify 处理；此处返回空串
    String::new()
}

/// 执行 JS 表达式，返回结果字符串
pub fn eval(expr: &str, ctx: &JsContext) -> AppResult<String> {
    eval_inner(expr, ctx, true)
}

/// 规则求值：表达式包进 IIFE，兼容 @js: 复杂的 replace/正则链（返回表达式结果）
fn eval_inner(expr: &str, ctx: &JsContext, script_mode: bool) -> AppResult<String> {
    let runtime = Runtime::new().map_err(|e| AppError::internal(format!("QuickJS runtime: {e}")))?;
    let context = Context::full(&runtime).map_err(|e| AppError::internal(format!("QuickJS context: {e}")))?;

    context.with(|js| {
        // 内置官方 CryptoJS（对齐 Legado）：每次新建 Runtime 后注入一次全局 CryptoJS。
        // 仅当脚本/jsLib 实际引用 CryptoJS 时才 eval，避免无谓开销。
        let uses_crypto = expr.contains("CryptoJS") || ctx.js_lib.contains("CryptoJS");
        if uses_crypto {
            let _: Result<Value, _> = js.eval(CRYPTO_JS_SOURCE);
        }
        inject_bindings(&js, ctx)?;
        let script = if script_mode {
            let expr_json = serde_json::to_string(expr).unwrap_or_else(|_| "\"\"".into());
            format!("{}\n;var __reader_result = eval({});\n__reader_result;", ctx.js_lib, expr_json)
        } else {
            format!("{}\n;var __reader_result = (function(){{ return ({}); }})();\n__reader_result;", ctx.js_lib, expr)
        };
        let val: Value = js.eval(script).map_err(|e| {
            let detail = catch_js_error(&js);
            AppError::bad_request(format!("JS 执行失败: {} · {}", format_js_err(&e), detail))
        })?;
        Ok(js_value_to_string(&val))
    })
}

/// 执行 JS 语句并返回结果（用于 @js: 含多条语句的场景）
pub fn eval_statement(expr: &str, ctx: &JsContext) -> AppResult<String> { eval(expr, ctx) }

/// 执行多语句脚本（jsUrl / <js>...</js> 前缀），返回最后表达式的值
pub fn eval_script(expr: &str, ctx: &JsContext) -> AppResult<String> { eval_inner(expr, ctx, true) }

/// 求值 jsUrl：解析 <js>...</js> 或 @js: 包装的脚本，返回最终 URL 字符串。
pub fn eval_js_url(raw: &str, ctx: &JsContext) -> AppResult<Option<String>> {
    let t = raw.trim();
    let expr = if t.to_ascii_lowercase().starts_with("<js>") && t.to_ascii_lowercase().ends_with("</js>") {
        t[4..t.len() - 5].trim()
    } else if t.to_ascii_lowercase().starts_with("@js:") {
        t[4..].trim()
    } else {
        return Ok(None);
    };
    Ok(Some(eval_inner(expr, ctx, true)?))
}

// ---------------- 进程级 cache（cache.put/get 共用） ----------------

type JsCacheMap = HashMap<String, (String, std::time::SystemTime)>;

static JS_CACHE: OnceLock<Mutex<JsCacheMap>> = OnceLock::new();

fn js_cache() -> &'static Mutex<JsCacheMap> {
    JS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------- Android Base64 flags（对齐 android.util.Base64） ----------------

/// flags: NO_PADDING=1, NO_WRAP=2, CRLF=4, URL_SAFE=8；DEFAULT(0) 每 76 字符换行
fn android_b64_encode(data: &[u8], flags: i32) -> String {
    use base64::Engine;
    let alphabet = if flags & 8 != 0 {
        base64::alphabet::URL_SAFE
    } else {
        base64::alphabet::STANDARD
    };
    let cfg = base64::engine::GeneralPurposeConfig::new()
        .with_encode_padding(flags & 1 == 0)
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    let engine = base64::engine::GeneralPurpose::new(&alphabet, cfg);
    let encoded = engine.encode(data);
    if flags & 2 != 0 {
        return encoded; // NO_WRAP
    }
    let sep = if flags & 4 != 0 { "\r\n" } else { "\n" };
    let bytes = encoded.as_bytes();
    let mut out = String::with_capacity(encoded.len() + bytes.len() / 76 * sep.len() + 2);
    for (i, chunk) in bytes.chunks(76).enumerate() {
        if i > 0 { out.push_str(sep); }
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
    }
    out
}

/// 容错解码：去除空白；padding 无关
fn android_b64_decode(s: &str, flags: i32) -> AppResult<Vec<u8>> {
    use base64::Engine;
    let alphabet = if flags & 8 != 0 {
        base64::alphabet::URL_SAFE
    } else {
        base64::alphabet::STANDARD
    };
    let cfg = base64::engine::GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    let engine = base64::engine::GeneralPurpose::new(&alphabet, cfg);
    let cleaned: String = s.chars().filter(|c| !matches!(c, '\n' | '\r' | '\t' | ' ')).collect();
    engine.decode(cleaned.as_bytes()).map_err(|e| AppError::bad_request(format!("Base64 解码失败: {}", e)))
}

// ---------------- 时间格式化（java.timeFormatUTC / java.timeFormat） ----------------

/// SimpleDateFormat 子集：yyyy MM dd HH mm ss；shift_hours 为时区偏移
fn format_time_ms(ts_ms: i64, fmt: &str, shift_hours: f64) -> String {
    use chrono::Datelike;
    use chrono::Timelike;
    let shifted = ts_ms + (shift_hours * 3600_000.0) as i64;
    let Some(dt) = chrono::DateTime::from_timestamp_millis(shifted) else { return String::new() };
    let (y, mo, d, h, mi, s) = (
        dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), dt.second(),
    );
    fmt.replace("yyyy", &format!("{:04}", y))
        .replace("MM", &format!("{:02}", mo))
        .replace("dd", &format!("{:02}", d))
        .replace("HH", &format!("{:02}", h))
        .replace("mm", &format!("{:02}", mi))
        .replace("ss", &format!("{:02}", s))
}

// ---------------- encodeURI（Java URLEncoder 语义） ----------------

/// 保留 A-Za-z0-9 .-*_，空格转 +，其余按指定 charset 百分号编码（大写 hex）
fn url_encoder_encode(s: &str, charset: &str) -> String {
    let enc = encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (bytes, _, _) = enc.encode(s);
    let mut out = String::with_capacity(bytes.len() * 3);
    for b in bytes.iter() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => out.push(*b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------- 高级 API 组装脚本 ----------------

/// 在 Rust 绑定之上组装 legado 语义的 JS API：
/// - Response 对象（body()/code()/header(k)/headers()/url()）
/// - java.ajax / ajaxAll / connect / get / post / head
/// - java.createSymmetricCrypto（encrypt/encryptBase64/encryptHex/decrypt/decryptStr）
/// - strToBytes/bytesToStr（ArrayBuffer）、base64DecodeToByteArray、hexDecodeToByteArray
/// - cache.put/get、getCookie(tag,key?)、timeFormat、encodeURI、htmlFormat、logType
const BOOTSTRAP_JS: &str = r#"(function() {
    if (typeof java === 'undefined') return;
    function normInput(u) { return typeof u === 'string' ? u : JSON.stringify(u); }
    function normHeaders(h) {
        if (h == null) return {};
        if (typeof h === 'string') return h;
        try { return JSON.stringify(h); } catch (e) { return {}; }
    }
    function parseResp(s) { try { return JSON.parse(s); } catch (e) { return { __err: String(e) }; } }
    function mkResp(o) {
        if (!o || o.__err) throw new Error(o && o.__err ? o.__err : '请求失败');
        const headers = o.headers || {};
        return {
            body: function() { return o.body; },
            code: function() { return o.status; },
            url: function() { return o.url; },
            header: function(k) { const v = headers[String(k).toLowerCase()]; return v === undefined ? null : v; },
            headers: function() { return headers; },
            toString: function() { return o.body; }
        };
    }
    function hexToAB(h) {
        const u = new Uint8Array(h.length >> 1);
        for (let i = 0; i < u.length; i++) u[i] = parseInt(h.substr(i * 2, 2), 16);
        return u.buffer;
    }
    function abToHex(ab) {
        const u = new Uint8Array(ab);
        let s = '';
        for (let i = 0; i < u.length; i++) s += ('0' + u[i].toString(16)).slice(-2);
        return s;
    }
    const hasFetch = typeof java.__fetch === 'function';

    // ---- 网络 ----
    java.ajax = function(url, timeout) {
        if (!hasFetch) return '';
        const o = parseResp(java.__fetch(normInput(url), '{}', timeout || 0));
        return o.__err ? '' : o.body;
    };
    java.ajaxAll = function(urls, timeout) {
        if (!hasFetch) return [];
        const arr = JSON.parse(java.__ajaxAll(JSON.stringify(urls || []), timeout || 0));
        return arr.map(function(o) { return mkResp(o); });
    };
    java.connect = function(url, headers, timeout) {
        if (!hasFetch) throw new Error('网络层不可用');
        return mkResp(parseResp(java.__fetch(normInput(url), JSON.stringify({ headers: normHeaders(headers) }), timeout || 0)));
    };
    java.get = function(url, headers, timeout) {
        if (!hasFetch) throw new Error('网络层不可用');
        return mkResp(parseResp(java.__fetch(normInput(url), JSON.stringify({ headers: normHeaders(headers) }), timeout || 0)));
    };
    java.post = function(url, body, headers, timeout) {
        if (!hasFetch) throw new Error('网络层不可用');
        const opts = { method: 'POST', body: body == null ? '' : String(body), headers: normHeaders(headers) };
        return mkResp(parseResp(java.__fetch(normInput(url), JSON.stringify(opts), timeout || 0)));
    };
    java.head = function(url, headers, timeout) {
        if (!hasFetch) throw new Error('网络层不可用');
        return mkResp(parseResp(java.__fetch(normInput(url), JSON.stringify({ method: 'HEAD', headers: normHeaders(headers) }), timeout || 0)));
    };

    // ---- Cookie ----
    java.getCookie = function(tag, key) {
        const c = typeof java.__getCookie === 'function' ? java.__getCookie(String(tag || '')) : '';
        if (key == null) return c;
        const prefix = key + '=';
        for (const p of c.split(/; */)) {
            if (p.startsWith(prefix)) return p.slice(prefix.length);
        }
        return '';
    };

    // ---- Base64 / hex / 字节 ----
    java.base64Encode = function(s, flags) {
        return typeof java.__b64Enc === 'function' ? java.__b64Enc(String(s == null ? '' : s), flags || 2) : '';
    };
    java.base64Decode = function(s, arg) {
        if (typeof java.__b64Dec !== 'function') return '';
        if (typeof arg === 'number') return java.__b64Dec(String(s || ''), arg);
        if (typeof arg === 'string' && arg) {
            const b = hexToAB(java.__b64DecHex(String(s || ''), 0));
            return java.__hexToStr(abToHex(b), arg);
        }
        return java.__b64Dec(String(s || ''), 0);
    };
    java.base64DecodeToByteArray = function(s, flags) {
        return hexToAB(java.__b64DecHex(String(s || ''), typeof flags === 'number' ? flags : 0));
    };
    java.hexEncodeToString = function(s) { return java.__hexEnc(String(s == null ? '' : s)); };
    java.hexDecodeToString = function(s) { return java.__hexDecStr(String(s || '')); };
    java.hexDecodeToByteArray = function(s) { return hexToAB(String(s || '')); };
    java.strToBytes = function(s, charset) { return hexToAB(java.__strToHex(String(s == null ? '' : s), charset || 'UTF-8')); };
    java.bytesToStr = function(ab, charset) {
        if (ab == null) return '';
        try { return java.__hexToStr(abToHex(ab), charset || 'UTF-8'); }
        catch (e) { return String(ab); }
    };

    // ---- 对称加密：createSymmetricCrypto ----
    java.createSymmetricCrypto = function(transformation, key, iv) {
        const t = String(transformation), k = String(key), v = iv == null ? '' : String(iv);
        const enc = function(out, d) {
            if (out === 'hex') return java.__symEncHex(t, k, v, String(d));
            if (out === 'raw') return java.__symEncRaw(t, k, v, String(d));
            return java.__symEncB64(t, k, v, String(d));
        };
        return {
            encrypt: function(d) { return hexToAB(enc('hex', d)); },
            encryptBase64: function(d) { return enc('b64', d); },
            encryptHex: function(d) { return enc('hex', d); },
            decrypt: function(d) { return hexToAB(java.__symDecHex(t, k, v, String(d))); },
            decryptStr: function(d) { return java.__symDecStr(t, k, v, String(d)); }
        };
    };

    // ---- cache ----
    const cacheObj = {
        put: function(k, v, saveTime) { return typeof java.__cachePut === 'function' ? java.__cachePut(String(k), v == null ? '' : String(v), saveTime || 0) : ''; },
        get: function(k) { return typeof java.__cacheGet === 'function' ? java.__cacheGet(String(k)) : null; }
    };
    java.cache = cacheObj;
    try { cache = cacheObj; } catch (e) { /* 已有全局声明则忽略 */ }

    // ---- 时间 / URL / HTML ----
    java.timeFormatUTC = function(time, fmt, sh) { return java.__fmtTime(Number(time) || 0, String(fmt || 'yyyy-MM-dd HH:mm:ss'), Number(sh) || 0); };
    java.timeFormat = function(time) { return java.__fmtTime(Number(time) || 0, 'yyyy-MM-dd HH:mm:ss', 0); };
    java.encodeURI = function(s, enc) { return java.__encodeURI(String(s == null ? '' : s), enc || 'UTF-8'); };
    java.htmlFormat = function(s, redirectUrl) { return java.__htmlFormat(String(s == null ? '' : s), redirectUrl == null ? '' : String(redirectUrl)); };

    // ---- 其它 ----
    java.logType = function(v) {
        if (v === null) return 'null';
        if (Array.isArray(v)) return 'array';
        return typeof v;
    };
    java.getWebViewUA = function() {
        return 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36';
    };
})();
"#;

fn format_js_err(e: &rquickjs::Error) -> String { format!("{e}") }

fn catch_js_error(js: &rquickjs::Ctx<'_>) -> String {
    let exc = js.catch();
    if let Some(obj) = exc.as_object() {
        let msg = obj.get::<_, Value>("message").ok()
            .map(|v| js_value_to_string(&v)).unwrap_or_default();
        let stack = obj.get::<_, Value>("stack").ok()
            .map(|v| js_value_to_string(&v)).unwrap_or_default();
        if !msg.is_empty() {
            return if stack.is_empty() { msg } else { format!("{} | {}", msg, stack.chars().take(300).collect::<String>()) };
        }
    }
    js_value_to_string(&exc)
}

/// 读取 VarStore 的 JSON 对象字符串
fn vars_json(store: &VarStore) -> String {
    let map = store.lock().ok().map(|g| g.clone()).unwrap_or_default();
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

fn inject_bindings(js: &rquickjs::Ctx<'_>, jc: &JsContext) -> AppResult<()> {
    js.globals().set("result", jc.result.clone()).ok();
    js.globals().set("baseUrl", jc.base_url.clone()).ok();
    js.globals().set("bookName", jc.book_name.clone()).ok();
    js.globals().set("title", jc.chapter_title.clone()).ok();
    js.globals().set("sourceUrl", jc.source_url.clone()).ok();

    let java = Object::new(js.clone()).map_err(|e| AppError::internal(format!("java obj: {e}")))?;

    // 网络层：__fetch(raw, optsJson, timeoutMs) → FetchResponse JSON（供 ajax/connect/get/post/head 用）
    if let Some(fetch) = &jc.fetch {
        let fetch_json = fetch.clone();
        let f = Function::new(js.clone(), move |raw: String, opts: String, timeout: f64| -> String {
            let t = if timeout > 0.0 { Some(timeout as u64) } else { None };
            let full = if opts.trim().is_empty() || opts.trim() == "{}" {
                raw
            } else {
                format!("{},{}", raw, opts)
            };
            match fetch_json(&full, "", t.unwrap_or(0)) {
                Ok(r) => r.to_json(),
                Err(e) => FetchResponse::error(&e).to_json(),
            }
        }).map_err(|e| AppError::internal(format!("__fetch: {e}")))?;
        java.set("__fetch", f).ok();

        // __ajaxAll(urlsJson, timeoutMs)：多线程并发抓取（对齐 java.ajaxAll）
        let fetch_all = fetch.clone();
        let f = Function::new(js.clone(), move |urls: String, timeout: f64| -> String {
            let t = if timeout > 0.0 { Some(timeout as u64) } else { None };
            let list: Vec<String> = serde_json::from_str(&urls).unwrap_or_default();
            let one = |f: &JsFetchFn, u: &str, t: Option<u64>| -> String {
                match f(u, "", t.unwrap_or(0)) {
                    Ok(r) => r.to_json(),
                    Err(e) => FetchResponse::error(&e).to_json(),
                }
            };
            let results: Vec<String> = if list.len() <= 8 {
                // 少量 URL：线程池开销大于收益，顺序执行
                list.iter().map(|u| one(&fetch_all, u, t)).collect()
            } else {
                // 大批量：std::thread::scope 并发，上限 16
                let mut out = Vec::with_capacity(list.len());
                for chunk in list.chunks(16) {
                    let handles: Vec<_> = chunk.iter().map(|u| {
                        let f = fetch_all.clone();
                        let u = u.clone();
                        std::thread::spawn(move || one(&f, &u, t))
                    }).collect();
                    out.extend(handles.into_iter().map(|h| h.join().unwrap_or_default()));
                }
                out
            };
            format!("[{}]", results.join(","))
        }).map_err(|e| AppError::internal(format!("__ajaxAll: {e}")))?;
        java.set("__ajaxAll", f).ok();
    }

    // base64（对齐 Android Base64 flags）/ log / getString
    use base64::Engine;
    let b64_enc_fn = Function::new(js.clone(), |s: String, flags: f64| -> String {
        android_b64_encode(s.as_bytes(), flags as i32)
    }).ok();
    if let Some(f) = b64_enc_fn { java.set("__b64Enc", f).ok(); }
    let b64_dec_fn = Function::new(js.clone(), |s: String, flags: f64| -> String {
        android_b64_decode(&s, flags as i32)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default()
    }).ok();
    if let Some(f) = b64_dec_fn { java.set("__b64Dec", f).ok(); }
    let b64_dec_hex_fn = Function::new(js.clone(), |s: String, flags: f64| -> String {
        android_b64_decode(&s, flags as i32).map(|b| hex::encode(b)).unwrap_or_default()
    }).ok();
    if let Some(f) = b64_dec_hex_fn { java.set("__b64DecHex", f).ok(); }
    let log = Function::new(js.clone(), |msg: Value| {
        let s = js_value_to_string(&msg);
        tracing::debug!(target: "js", "{}", s);
    }).ok();
    if let Some(f) = log { java.set("log", f).ok(); }
    let to_str = Function::new(js.clone(), |v: Value| -> String { js_value_to_string(&v) }).ok();
    if let Some(f) = to_str { java.set("getString", f).ok(); }

    // hex 工具（hexEncodeToString / hexDecodeToString / hexDecodeToByteArray）
    let hex_enc_fn = Function::new(js.clone(), |s: String| -> String { hex::encode(s.as_bytes()) }).ok();
    if let Some(f) = hex_enc_fn { java.set("__hexEnc", f).ok(); }
    let hex_dec_fn = Function::new(js.clone(), |s: String| -> String {
        hex::decode(s.trim()).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default()
    }).ok();
    if let Some(f) = hex_dec_fn { java.set("__hexDecStr", f).ok(); }

    // 字节工具：strToBytes / bytesToStr（经 hex 中转，JS 侧转 ArrayBuffer）
    let str_to_hex_fn = Function::new(js.clone(), |s: String, charset: String| -> String {
        let enc = encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
        let (bytes, _, _) = enc.encode(&s);
        hex::encode(bytes)
    }).ok();
    if let Some(f) = str_to_hex_fn { java.set("__strToHex", f).ok(); }
    let hex_to_str_fn = Function::new(js.clone(), |s: String, charset: String| -> String {
        match hex::decode(s.trim()) {
            Ok(b) => {
                let enc = encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
                let (cow, _, _) = enc.decode(&b);
                cow.into_owned()
            }
            Err(_) => String::new(),
        }
    }).ok();
    if let Some(f) = hex_to_str_fn { java.set("__hexToStr", f).ok(); }

    // java.getCookie(tag) —— 真实 CookieStore 查询（不再恒空）
    let cookie_store = jc.cookies.clone();
    let cookie_fn = Function::new(js.clone(), move |tag: String| -> String {
        cookie_store.get_by_tag(&tag)
    }).ok();
    if let Some(f) = cookie_fn { java.set("__getCookie", f).ok(); }

    // 进程级缓存（对齐 cache.put/get；内存版，saveTime 秒过期）
    let cache_put = Function::new(js.clone(), |k: String, v: String, save_time: f64| -> String {
        let mut m = js_cache().lock().unwrap_or_else(|e| e.into_inner());
        let expiry = if save_time > 0.0 {
            std::time::SystemTime::now() + std::time::Duration::from_secs(save_time as u64)
        } else {
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 << 40)
        };
        m.insert(k, (v.clone(), expiry));
        v
    }).ok();
    if let Some(f) = cache_put { java.set("__cachePut", f).ok(); }
    let cache_get = Function::new(js.clone(), |k: String| -> String {
        let now = std::time::SystemTime::now();
        let mut m = js_cache().lock().unwrap_or_else(|e| e.into_inner());
        match m.get(&k) {
            Some((v, exp)) if *exp > now => v.clone(),
            Some(_) => { m.remove(&k); String::new() }
            None => String::new(),
        }
    }).ok();
    if let Some(f) = cache_get { java.set("__cacheGet", f).ok(); }

    // 时间格式化（timeFormatUTC / timeFormat）
    let fmt_fn = Function::new(js.clone(), |time: f64, fmt: String, shift_hours: f64| -> String {
        format_time_ms(time as i64, &fmt, shift_hours)
    }).ok();
    if let Some(f) = fmt_fn { java.set("__fmtTime", f).ok(); }

    // encodeURI（Java URLEncoder 语义：空格→+，保留 .-*_）
    let enc_uri_fn = Function::new(js.clone(), |s: String, charset: String| -> String {
        url_encoder_encode(&s, &charset)
    }).ok();
    if let Some(f) = enc_uri_fn { java.set("__encodeURI", f).ok(); }

    // htmlFormat（对齐 HtmlFormatter.formatKeepImg）
    let html_fmt_fn = Function::new(js.clone(), |s: String, redirect_url: String| -> String {
        crate::engine::htmlfmt::format_keep_img(&s, if redirect_url.is_empty() { None } else { Some(&redirect_url) })
    }).ok();
    if let Some(f) = html_fmt_fn { java.set("__htmlFormat", f).ok(); }

    // randomUUID
    let uuid_fn = Function::new(js.clone(), || -> String { uuid::Uuid::new_v4().to_string() }).ok();
    if let Some(f) = uuid_fn { java.set("randomUUID", f).ok(); }

    // importScript(path)：http(s) 直接抓取，相对路径按书源 URL 解析
    if let Some(fetch) = &jc.fetch {
        let base = if jc.source_url.is_empty() { jc.base_url.clone() } else { jc.source_url.clone() };
        let fetch_imp = fetch.clone();
        let f = Function::new(js.clone(), move |path: String| -> String {
            let url = if path.starts_with("http://") || path.starts_with("https://") {
                path
            } else {
                url::Url::parse(&base).ok()
                    .and_then(|b| b.join(path.trim()).ok())
                    .map(|u| u.to_string())
                    .unwrap_or(path)
            };
            fetch_imp(&url, "", 0).map(|r| r.body).unwrap_or_default()
        }).ok();
        if let Some(f) = f { java.set("importScript", f).ok(); }
    }

    // java.put / java.get —— 写入共享变量表（Rust 闭包，真正回传给规则层与落库）
    let put_store = jc.variables.clone();
    let put_fn = Function::new(js.clone(), move |k: String, v: Value| -> String {
        let s = js_value_to_string(&v);
        if let Ok(mut m) = put_store.lock() { m.insert(k, s.clone()); }
        s
    }).ok();
    if let Some(f) = put_fn { java.set("put", f).ok(); }
    let get_store = jc.variables.clone();
    let get_fn = Function::new(js.clone(), move |k: String| -> String {
        get_store.lock().ok().and_then(|m| m.get(&k).cloned()).unwrap_or_default()
    }).ok();
    if let Some(f) = get_fn { java.set("get", f).ok(); }

    // 加密扩展：对齐 Legado JsEncodeUtils（摘要 / HMac / AES）
    inject_crypto_bindings(js, &java);

    js.globals().set("java", java).ok();

    // jsUrl 变量：key / page
    js.globals().set("key", jc.key.clone()).ok();
    js.globals().set("page", jc.page).ok();

    // JSON 规则中的内联 {{$.field}}：注入 $ = JSON.parse(result)
    let dollar_bootstrap = format!(
        "var $ = (function(){{ try{{ return JSON.parse({}); }}catch(e){{ return {{}}; }} }})();",
        serde_json::to_string(&jc.result).unwrap_or_else(|_| "\"\"".into())
    );
    let _: Result<Value, _> = js.eval(dollar_bootstrap);

    // cookie 占位对象（真实 cookie 走 java.getCookie）；cache 在 bootstrap 中挂载为真实实现
    let cookie_obj = Object::new(js.clone()).ok();
    if let Some(o) = cookie_obj {
        js.globals().set("cookie", o).ok();
    }

    // 高级 API 组装：Response 对象 / ajax 家族 / createSymmetricCrypto / 字节转换 / cache
    let _: Result<Value, _> = js.eval(BOOTSTRAP_JS);

    // source 对象：getVariable / setVariable / getLoginInfoMap / 书源信息
    let source = Object::new(js.clone()).ok();
    if let Some(s) = source {
        s.set("bookSourceUrl", jc.source_url.clone()).ok();
        s.set("bookSourceName", jc.source_name.clone()).ok();

        // getVariable：返回共享变量表的 JSON 字符串（书源通常 getVariable()['key']）
        let get_var_store = jc.variables.clone();
        let get_variable = Function::new(js.clone(), move || -> String { vars_json(&get_var_store) }).ok();
        if let Some(f) = get_variable { s.set("getVariable", f).ok(); }

        // setVariable：Rust 闭包只接收字符串；JS 包装层负责把对象 JSON.stringify
        let set_var_store = jc.variables.clone();
        let set_variable = Function::new(js.clone(), move |raw: String| -> String {
            let obj: serde_json::Value = serde_json::from_str(&raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
            if let Some(map) = obj.as_object() {
                if let Ok(mut m) = set_var_store.lock() {
                    for (k, val) in map {
                        m.insert(k.clone(), match val {
                            serde_json::Value::String(x) => x.clone(),
                            other => other.to_string(),
                        });
                    }
                }
            }
            raw
        }).ok();
        if let Some(f) = set_variable { s.set("__readerSetVar", f).ok(); }
        let _: Result<Value, _> = js.eval(
            "source.setVariable = function(v){ return __readerSetVar(typeof v === 'string' ? v : JSON.stringify(v)); };"
        );

        // getLoginInfoMap().get(k)：注入登录信息快照（书源只读场景足够）
        let login_snapshot = vars_json(&jc.login_info);
        let login_bootstrap = format!(
            "var __LOGIN = {}; \
             source.getLoginInfoMap = function() {{ \
               return {{ get: function(k) {{ \
                 return Object.prototype.hasOwnProperty.call(__LOGIN, k) ? __LOGIN[k] : null; \
               }} }}; \
             }};",
            login_snapshot
        );
        let _: Result<Value, _> = js.eval(login_bootstrap);

        js.globals().set("source", s).ok();
    }

    Ok(())
}

/// 注入 java.* 加密方法，对齐 Legado `JsEncodeUtils.kt`：
/// - md5Encode / md5Encode16
/// - digestHex / digestBase64Str(data, algorithm)
/// - HMacHex / HMacBase64(data, algorithm, key)
/// - aesEncodeToString / aesEncodeToBase64String / aesEncodeArgsBase64Str
/// - aesDecodeToString / aesBase64DecodeToString / aesDecodeArgsBase64Str
fn inject_crypto_bindings<'js>(js: &rquickjs::Ctx<'js>, java: &Object<'js>) {
    use crate::engine::crypto as c;

    macro_rules! bind1 {
        ($name:expr, $f:expr) => {
            if let Some(f) = Function::new(js.clone(), move |s: String| -> String {
                $f(&s).unwrap_or_default()
            }).ok() { java.set($name, f).ok(); }
        };
    }
    macro_rules! bind2 {
        ($name:expr, $f:expr) => {
            if let Some(f) = Function::new(js.clone(), move |a: String, b: String| -> String {
                $f(&a, &b).unwrap_or_default()
            }).ok() { java.set($name, f).ok(); }
        };
    }
    macro_rules! bind3 {
        ($name:expr, $f:expr) => {
            if let Some(f) = Function::new(js.clone(), move |a: String, b: String, c3: String| -> String {
                $f(&a, &b, &c3).unwrap_or_default()
            }).ok() { java.set($name, f).ok(); }
        };
    }

    bind1!("md5Encode", |s: &String| -> AppResult<String> { Ok(c::md5_encode(s)) });
    bind1!("md5Encode16", |s: &String| -> AppResult<String> { Ok(c::md5_encode16(s)) });
    bind2!("digestHex", c::digest_hex);
    bind2!("digestBase64Str", c::digest_base64);
    bind3!("HMacHex", c::hmac_hex);
    bind3!("HMacBase64", c::hmac_base64);

    // AES：data, key, transformation, iv（key/iv 为原始字符串字节）
    // aesEncodeToString 官方语义是"加密后 Base64"（aesEncodeToBase64String 的别名）
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, t: String, iv: String| -> String {
        c::aes_encrypt_base64(&data, &key, &t, &iv, false).unwrap_or_default()
    }).ok() { java.set("aesEncodeToString", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, t: String, iv: String| -> String {
        c::aes_encrypt_base64(&data, &key, &t, &iv, false).unwrap_or_default()
    }).ok() { java.set("aesEncodeToBase64String", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, t: String, iv: String| -> String {
        c::aes_encrypt_hex(&data, &key, &t, &iv, false).unwrap_or_default()
    }).ok() { java.set("aesEncodeToHex", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, t: String, iv: String| -> String {
        c::aes_decrypt_base64_to_string(&data, &key, &t, &iv, false).unwrap_or_default()
    }).ok() { java.set("aesDecodeToString", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, t: String, iv: String| -> String {
        c::aes_decrypt_base64_to_string(&data, &key, &t, &iv, false).unwrap_or_default()
    }).ok() { java.set("aesBase64DecodeToString", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, t: String, iv: String| -> String {
        c::aes_decrypt_hex_to_string(&data, &key, &t, &iv, false).unwrap_or_default()
    }).ok() { java.set("aesDecodeHexToString", f).ok(); }

    // ArgsBase64 变体：key/iv 为 Base64（对齐 aesEncodeArgsBase64Str / aesDecodeArgsBase64Str）
    // 签名 (data, key, mode, padding, iv)：mode+padding 拼成 transformation
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, mode: String, padding: String, iv: String| -> String {
        let t = format!("AES/{}/{}", mode, padding);
        c::aes_encrypt_base64(&data, &key, &t, &iv, true).unwrap_or_default()
    }).ok() { java.set("aesEncodeArgsBase64Str", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |data: String, key: String, mode: String, padding: String, iv: String| -> String {
        let t = format!("AES/{}/{}", mode, padding);
        c::aes_decrypt_base64_to_string(&data, &key, &t, &iv, true).unwrap_or_default()
    }).ok() { java.set("aesDecodeArgsBase64Str", f).ok(); }

    // ---- createSymmetricCrypto 的 Rust 底座：__symEnc{B64,Hex,Raw} / __symDec{Str,Hex} ----
    // transformation 覆盖 AES/DES/DESede，由 crypto::parse_transformation 统一分发
    macro_rules! bind5s {
        ($name:expr, $f:expr) => {
            if let Some(f) = Function::new(js.clone(), move |a: String, b: String, c3: String, d: String, e: String| -> String {
                $f(&a, &b, &c3, &d, &e).unwrap_or_default()
            }).ok() { java.set($name, f).ok(); }
        };
    }
    if let Some(f) = Function::new(js.clone(), |t: String, k: String, iv: String, d: String| -> String {
        c::sym_encrypt_base64(&d, &k, &t, &iv).unwrap_or_default()
    }).ok() { java.set("__symEncB64", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |t: String, k: String, iv: String, d: String| -> String {
        c::sym_encrypt_hex(&d, &k, &t, &iv).unwrap_or_default()
    }).ok() { java.set("__symEncHex", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |t: String, k: String, iv: String, d: String| -> String {
        c::sym_encrypt_raw_string(&d, &k, &t, &iv).unwrap_or_default()
    }).ok() { java.set("__symEncRaw", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |t: String, k: String, iv: String, d: String| -> String {
        c::sym_decrypt_str(&d, &k, &t, &iv).unwrap_or_default()
    }).ok() { java.set("__symDecStr", f).ok(); }
    if let Some(f) = Function::new(js.clone(), |t: String, k: String, iv: String, d: String| -> String {
        c::sym_decrypt_auto(&d, &k, &t, &iv).map(|b| hex::encode(b)).unwrap_or_default()
    }).ok() { java.set("__symDecHex", f).ok(); }

    // ---- 官方遗留 des* / tripleDES* 函数 ----
    macro_rules! bind4 {
        ($name:expr, $f:expr) => {
            if let Some(f) = Function::new(js.clone(), move |a: String, b: String, c3: String, d: String| -> String {
                $f(&a, &b, &c3, &d).unwrap_or_default()
            }).ok() { java.set($name, f).ok(); }
        };
    }
    bind4!("desEncodeToString", c::des_encode_to_string);
    bind4!("desEncodeToBase64String", c::des_encode_to_base64_string);
    bind4!("desDecodeToString", c::des_decode_to_string);
    bind4!("desBase64DecodeToString", c::des_decode_to_string);
    // tripleDES*(data, key, mode, padding, iv)
    bind5s!("tripleDESEncodeBase64Str", |d: &str, k: &str, m: &str, p: &str, iv: &str| -> AppResult<String> {
        c::tripledes_encode_base64(d, k, m, p, iv)
    });
    bind5s!("tripleDESDecodeStr", |d: &str, k: &str, m: &str, p: &str, iv: &str| -> AppResult<String> {
        c::tripledes_decode_str(d, k, m, p, iv)
    });
    bind5s!("tripleDESEncodeArgsBase64Str", |d: &str, k: &str, m: &str, p: &str, iv: &str| -> AppResult<String> {
        c::tripledes_encode_args_base64(d, k, m, p, iv)
    });
    bind5s!("tripleDESDecodeArgsBase64Str", |d: &str, k: &str, m: &str, p: &str, iv: &str| -> AppResult<String> {
        c::tripledes_decode_args_base64(d, k, m, p, iv)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> JsContext { JsContext::default() }

    #[test]
    fn java_md5_bindings() {
        let c = ctx();
        assert_eq!(eval("java.md5Encode('abc')", &c).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(eval("java.md5Encode16('abc')", &c).unwrap(), "3cd24fb0d6963f7d");
        assert_eq!(eval("java.digestHex('abc','SHA-256')", &c).unwrap(),
                   "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn java_hmac_binding() {
        let c = ctx();
        assert_eq!(eval("java.HMacHex('The quick brown fox jumps over the lazy dog','md5','key')", &c).unwrap(),
                   "80070713463e7749b90c2dc24911e275");
    }

    // ---------------- 新增 API 测试 ----------------

    #[test]
    fn java_base64_flags() {
        let c = ctx();
        // 默认 NO_WRAP（对齐官方 EncoderUtils.base64Encode）
        assert_eq!(eval("java.base64Encode('hello')", &c).unwrap(), "aGVsbG8=");
        assert_eq!(eval("java.base64Decode('aGVsbG8=')", &c).unwrap(), "hello");
        // NO_PADDING
        assert_eq!(eval("java.base64Encode('hello', 3)", &c).unwrap(), "aGVsbG8");
        // URL_SAFE
        assert_eq!(eval("java.base64Encode('a+b/c', 10)", &c).unwrap(), "YStiL2M=");
        assert_eq!(eval("java.base64Decode('YStiL2M=', 8)", &c).unwrap(), "a+b/c");
        // 容错：带换行的 Base64
        assert_eq!(eval("java.base64Decode('aGVs\\nbG8=')", &c).unwrap(), "hello");
    }

    #[test]
    fn java_hex_and_bytes() {
        let c = ctx();
        assert_eq!(eval("java.hexEncodeToString('AB')", &c).unwrap(), "4142");
        assert_eq!(eval("java.hexDecodeToString('4142')", &c).unwrap(), "AB");
        // 字节工具（ArrayBuffer 往返）
        assert_eq!(
            eval("java.bytesToStr(java.strToBytes('你好', 'UTF-8'), 'UTF-8')", &c).unwrap(),
            "你好"
        );
        assert_eq!(
            eval("java.bytesToStr(java.strToBytes('你好', 'UTF-8'), 'GBK')", &c).unwrap(),
            "浣犲ソ"
        ); // GBK 字节按 UTF-8 解读必然乱码，验证 charset 传参生效
        assert_eq!(
            eval("java.bytesToStr(java.base64DecodeToByteArray(java.base64Encode('xyz')), 'UTF-8')", &c).unwrap(),
            "xyz"
        );
    }

    #[test]
    fn java_create_symmetric_crypto_aes() {
        let c = ctx();
        let script = r#"
            const crypto = java.createSymmetricCrypto('AES/CBC/PKCS5Padding', '1234567890abcdef', '8765432187654321');
            const b64 = crypto.encryptBase64('secret');
            crypto.decryptStr(b64);
        "#;
        assert_eq!(eval(script, &c).unwrap(), "secret");
        // decrypt(String) 自动识别 hex / Base64
        let script2 = r#"
            const crypto = java.createSymmetricCrypto('AES/ECB/PKCS5Padding', '1234567890abcdef', '');
            const hex = crypto.encryptHex('abc');
            const b64 = crypto.encryptBase64('abc');
            [crypto.decryptStr(hex), crypto.decryptStr(b64)].join('|');
        "#;
        assert_eq!(eval(script2, &c).unwrap(), "abc|abc");
    }

    #[test]
    fn java_create_symmetric_crypto_des() {
        let c = ctx();
        // DES 8 字节密钥往返
        let script = r#"
            const crypto = java.createSymmetricCrypto('DES/ECB/PKCS5Padding', '12345678', '');
            crypto.decryptStr(crypto.encryptBase64('des-test'));
        "#;
        assert_eq!(eval(script, &c).unwrap(), "des-test");
        // 3DES 16 字节密钥往返（K1K2K1）
        let script2 = r#"
            const crypto = java.createSymmetricCrypto('DESede/CBC/PKCS5Padding', '1234567890abcdef', '12345678');
            crypto.decryptStr(crypto.encryptBase64('triple-des'));
        "#;
        assert_eq!(eval(script2, &c).unwrap(), "triple-des");
    }

    #[test]
    fn java_des_legacy_bindings() {
        let c = ctx();
        assert_eq!(
            eval("java.desDecodeToString(java.desEncodeToBase64String('legacy', '12345678', 'DES/ECB/PKCS5Padding', ''), '12345678', 'DES/ECB/PKCS5Padding', '')", &c).unwrap(),
            "legacy"
        );
        assert_eq!(
            eval("java.tripleDESDecodeStr(java.tripleDESEncodeBase64Str('t', '1234567890abcdef12345678', 'ECB', 'PKCS5Padding', ''), '1234567890abcdef12345678', 'ECB', 'PKCS5Padding', '')", &c).unwrap(),
            "t"
        );
    }

    #[test]
    fn java_cache_put_get() {
        let c = ctx();
        assert_eq!(eval("java.cache.put('k1', 'v1'); java.cache.get('k1')", &c).unwrap(), "v1");
        // 过期：saveTime=1s 内可读，此处仅验证 put 返回值
        assert_eq!(eval("java.cache.put('k2', 'v2', 60)", &c).unwrap(), "v2");
        assert_eq!(eval("java.cache.get('not-exist')", &c).unwrap(), "");
    }

    #[test]
    fn java_time_and_uri() {
        let c = ctx();
        // UTC+0 2024-01-02 03:04:05
        assert_eq!(
            eval("java.timeFormatUTC(1704164645000, 'yyyy-MM-dd HH:mm:ss', 0)", &c).unwrap(),
            "2024-01-02 03:04:05"
        );
        assert_eq!(
            eval("java.timeFormatUTC(1704164645000, 'yyyy-MM-dd HH:mm:ss', 8)", &c).unwrap(),
            "2024-01-02 11:04:05"
        );
        assert_eq!(eval("java.encodeURI('a b*c-._d', 'UTF-8')", &c).unwrap(), "a+b*c-._d");
        assert_eq!(eval("java.encodeURI('中', 'UTF-8')", &c).unwrap(), "%E4%B8%AD");
    }

    #[test]
    fn java_html_format() {
        let c = ctx();
        let script = r#"
            java.htmlFormat('<div>第一段</div><p>第二段<br>换行</p>tail')
        "#;
        assert_eq!(eval(script, &c).unwrap(), "第一段\n　　第二段\n　　换行\n　　tail");
    }

    #[test]
    fn java_misc_bindings() {
        let c = ctx();
        assert_eq!(eval("java.logType([1,2])", &c).unwrap(), "array");
        assert_eq!(eval("java.logType('s')", &c).unwrap(), "string");
        assert_eq!(eval("java.randomUUID().length", &c).unwrap(), "36");
        assert!(eval("java.getWebViewUA().includes('Chrome')", &c).unwrap() == "true");
        // getCookie(tag, key)
        assert_eq!(eval("java.getCookie('t', 'k')", &c).unwrap(), "");
    }

    #[test]
    fn java_connect_response_object() {
        // 无 fetch 闭包时应抛错（网络层不可用）
        let c = ctx();
        let r = eval("java.connect('http://x')", &c);
        assert!(r.is_err());
    }

    #[test]
    fn java_connect_get_post_with_stub_fetch() {
        // 带 stub fetch 闭包：验证 connect/get/post/ajaxAll 的 Response 语义
        let mut c = ctx();
        c.fetch = Some(Arc::new(|raw: &str, _h: &str, timeout: u64| {
            let url = raw.split(',').next().unwrap_or("");
            let mut headers = serde_json::Map::new();
            headers.insert("content-type".to_string(), serde_json::Value::String("text/html".into()));
            Ok(FetchResponse {
                status: 200,
                headers,
                body: format!("body:{}:timeout={}", url, timeout),
                url: url.to_string(),
            })
        }));
        assert_eq!(eval("java.ajax('http://a.com')", &c).unwrap(), "body:http://a.com:timeout=0");
        assert_eq!(eval("java.ajax('http://a.com', 5000)", &c).unwrap(), "body:http://a.com:timeout=5000");
        let script = r#"
            const r = java.connect('http://b.com', {'User-Agent': 'test'}, 3000);
            [r.body(), r.code(), r.header('Content-Type'), r.url()].join('|');
        "#;
        assert_eq!(eval(script, &c).unwrap(), "body:http://b.com:timeout=3000|200|text/html|http://b.com");
        let post_script = r#"
            const r = java.post('http://p.com', 'k=v', {'Content-Type': 'application/x-www-form-urlencoded'});
            r.body();
        "#;
        assert_eq!(eval(post_script, &c).unwrap(), "body:http://p.com:timeout=0");
        assert_eq!(
            eval("java.ajaxAll(['http://1.com','http://2.com']).map(r => r.body()).join(',')", &c).unwrap(),
            "body:http://1.com:timeout=0,body:http://2.com:timeout=0"
        );
    }

    #[test]
    fn java_aes_binding() {
        let c = ctx();
        let expr = "java.aesDecodeToString(java.aesEncodeToString('hello','1234567890abcdef','AES/ECB/PKCS5Padding',''),'1234567890abcdef','AES/ECB/PKCS5Padding','')";
        assert_eq!(eval(expr, &c).unwrap(), "hello");
    }

    #[test]
    fn builtin_cryptojs_injected() {
        let c = ctx();
        // 官方 cryptojs.min.js 应在脚本引用 CryptoJS 时自动注入
        assert_eq!(eval("CryptoJS.MD5('abc').toString()", &c).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(eval("CryptoJS.SHA256('abc').toString()", &c).unwrap(),
                   "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
