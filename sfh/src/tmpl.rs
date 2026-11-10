// tmpl.rs — the two panel templates, kept byte-identical to the original
// Go html/template sources, plus a minimal renderer implementing exactly the
// constructs they use, with Go html/template's contextual auto-escaping
// (htmlEscaper, urlFilter + urlNormalizer for href, jsStrEscaper for onclick
// JS-string literals, raw template.JS inside <script>).

use std::rc::Rc;

// ─── templates (verbatim from the original main.go) ─────────────────────────

pub const DIR_TMPL: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{.Path}} — sfh</title>
<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%232563eb'/%3E%3Ctext x='16' y='22' font-family='monospace' font-weight='700' font-size='14' fill='white' text-anchor='middle'%3Esfh%3C/text%3E%3C/svg%3E">
<style>
@import url('https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500&family=Inter:wght@300;400;500&display=swap');
*{box-sizing:border-box;margin:0;padding:0}
:root{
  --bg:#f8f9fb;--surf:#ffffff;--bdr:#e5e8ef;--bdr2:#d1d5de;
  --acc:#2563eb;--grn:#059669;--txt:#111827;--sub:#6b7280;
  --hvr:#f1f4f9;
}
body{background:var(--bg);color:var(--txt);font-family:'Inter',sans-serif;min-height:100vh;font-size:14px}
header{
  background:var(--surf);border-bottom:1px solid var(--bdr);
  padding:0 28px;height:52px;display:flex;align-items:center;gap:14px;
  position:sticky;top:0;z-index:10;
}
.logo{font-family:'JetBrains Mono',monospace;font-weight:500;font-size:.95rem;color:var(--acc);letter-spacing:.02em;flex-shrink:0}
.logo em{color:var(--grn);font-style:normal}
.bc{flex:1;display:flex;align-items:center;gap:2px;font-family:'JetBrains Mono',monospace;font-size:.78rem;min-width:0;overflow:hidden}
.bc a{color:var(--acc);text-decoration:none;padding:3px 6px;border-radius:4px;transition:.12s;white-space:nowrap}
.bc a:hover{background:var(--hvr);color:var(--grn)}
.bc .s{color:var(--bdr2);padding:0 1px}
main{max-width:860px;margin:32px auto;padding:0 20px}
.toolbar{display:flex;align-items:center;justify-content:space-between;margin-bottom:12px}
.info{font-size:.75rem;font-family:'JetBrains Mono',monospace;color:var(--sub)}
.card{background:var(--surf);border:1px solid var(--bdr);border-radius:8px;overflow:hidden}
table{width:100%;border-collapse:collapse}
th{
  text-align:left;padding:8px 14px;font-size:.7rem;font-weight:500;
  color:var(--sub);text-transform:uppercase;letter-spacing:.08em;
  border-bottom:1px solid var(--bdr);background:#fafbfc;
  font-family:'JetBrains Mono',monospace;
}
td{padding:9px 14px;border-bottom:1px solid var(--bdr);vertical-align:middle}
tr:last-child td{border-bottom:none}
tr:hover td{background:var(--hvr)}
.nm-cell{display:flex;align-items:center;gap:8px}
.ic{font-size:.95rem;flex-shrink:0}
a.nm{color:var(--txt);text-decoration:none;font-size:.88rem;transition:.12s}
a.nm:hover{color:var(--acc)}
a.nm.dir{color:var(--acc);font-weight:500}
a.nm.dir:hover{color:var(--grn)}
.sz,.mt{color:var(--sub);font-family:'JetBrains Mono',monospace;font-size:.75rem;white-space:nowrap}
.rb{
  display:inline-block;font-size:.63rem;font-family:'JetBrains Mono',monospace;
  background:#eff2f7;color:var(--sub);padding:1px 5px;border-radius:3px;
  margin-left:4px;vertical-align:middle;border:1px solid var(--bdr);
}
.empty{text-align:center;padding:56px;color:var(--sub);font-family:'JetBrains Mono',monospace;font-size:.85rem}
</style>
</head>
<body>
<header>
  <span class="logo">sfh<em>.</em></span>
  <div class="bc">
    {{range $i,$c:=.Crumbs}}{{if $i}}<span class="s">/</span>{{end}}<a href="{{$c.URL}}">{{$c.Name}}</a>{{end}}
  </div>
</header>
<main>
  <div class="toolbar">
    <span class="info">{{len .Files}} items</span>
  </div>
  <div class="card">
    {{if .Files}}
    <table>
      <thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead>
      <tbody>
      {{range .Files}}<tr>
        <td>
          <div class="nm-cell">
            <span class="ic">{{if .IsDir}}📁{{else}}📄{{end}}</span>
            {{if .IsDir}}
              <a class="nm dir" href="{{joinDir $.Path .Name}}">{{.Name}}</a>
            {{else}}
              <a class="nm" href="{{join $.Path .Name}}">{{.Name}}</a>
              <span class="rb">raw</span>
            {{end}}
          </div>
        </td>
        <td class="sz">{{.Size}}</td>
        <td class="mt">{{.ModTime}}</td>
      </tr>{{end}}
      </tbody>
    </table>
    {{else}}<div class="empty">— empty directory —</div>{{end}}
  </div>
</main>
</body></html>"##;

pub const UPD_TMPL: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>manage {{.Path}} — sfh</title>
<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%232563eb'/%3E%3Ctext x='16' y='22' font-family='monospace' font-weight='700' font-size='14' fill='white' text-anchor='middle'%3Esfh%3C/text%3E%3C/svg%3E">
<style>
@import url('https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500&family=Inter:wght@300;400;500&display=swap');
*{box-sizing:border-box;margin:0;padding:0}
:root{
  --bg:#f8f9fb;--surf:#ffffff;--s2:#f3f5f8;--bdr:#e5e8ef;--bdr2:#d1d5de;
  --acc:#2563eb;--grn:#059669;--txt:#111827;--sub:#6b7280;
  --hvr:#f1f4f9;--red:#dc2626;
}
body{background:var(--bg);color:var(--txt);font-family:'Inter',sans-serif;min-height:100vh;font-size:14px}
header{
  background:var(--surf);border-bottom:1px solid var(--bdr);
  padding:0 28px;height:52px;display:flex;align-items:center;gap:12px;
  position:sticky;top:0;z-index:10;
}
.logo{font-family:'JetBrains Mono',monospace;font-weight:500;font-size:.95rem;color:var(--acc)}
.logo em{color:var(--grn);font-style:normal}
.badge{font-size:.62rem;font-family:'JetBrains Mono',monospace;border:1px solid var(--acc);color:var(--acc);padding:2px 7px;border-radius:20px;opacity:.7}
.bc{flex:1;display:flex;align-items:center;gap:2px;font-family:'JetBrains Mono',monospace;font-size:.78rem;margin-left:4px;min-width:0;overflow:hidden}
.bc a{color:var(--acc);text-decoration:none;padding:3px 6px;border-radius:4px;transition:.12s;white-space:nowrap}
.bc a:hover{background:var(--hvr);color:var(--grn)}
.bc .s{color:var(--bdr2);padding:0 1px}
.vw{font-size:.75rem;font-family:'JetBrains Mono',monospace;border:1px solid var(--bdr2);color:var(--sub);padding:4px 11px;border-radius:5px;text-decoration:none;transition:.15s;white-space:nowrap;flex-shrink:0}
.vw:hover{border-color:var(--grn);color:var(--grn)}
.wrap{display:flex;gap:20px;max-width:1160px;margin:24px auto;padding:0 20px}
.side{width:248px;flex-shrink:0}
.main{flex:1;min-width:0}
.card{background:var(--surf);border:1px solid var(--bdr);border-radius:8px;padding:16px;margin-bottom:16px}
.card h3{font-size:.65rem;text-transform:uppercase;letter-spacing:.1em;color:var(--sub);font-family:'JetBrains Mono',monospace;margin-bottom:12px;font-weight:500}
.dz{
  border:2px dashed var(--bdr2);border-radius:6px;padding:20px 10px;
  text-align:center;cursor:pointer;color:var(--sub);font-size:.8rem;
  transition:.15s;position:relative;user-select:none;
}
.dz:hover,.dz.ov{border-color:var(--acc);color:var(--acc);background:rgba(37,99,235,.04)}
.dz .ic{font-size:1.6rem;display:block;margin-bottom:4px}
.dz input{position:absolute;inset:0;opacity:0;cursor:pointer;width:100%;height:100%}
input[type=text]{
  width:100%;background:var(--s2);border:1px solid var(--bdr);
  color:var(--txt);padding:7px 10px;border-radius:5px;
  font-family:'JetBrains Mono',monospace;font-size:.8rem;outline:none;transition:.12s;
}
input[type=text]:focus{border-color:var(--acc)}
.btn{display:inline-flex;align-items:center;gap:4px;padding:5px 12px;border-radius:5px;font-size:.76rem;font-family:'JetBrains Mono',monospace;cursor:pointer;border:none;transition:.15s;font-weight:500;text-decoration:none}
.bp{background:var(--acc);color:#fff}.bp:hover{opacity:.85}
.bg{background:var(--grn);color:#fff}.bg:hover{opacity:.85}
.bgh{background:transparent;border:1px solid var(--bdr2);color:var(--sub)}.bgh:hover{border-color:var(--acc);color:var(--acc)}
.br{background:transparent;border:1px solid var(--bdr2);color:var(--red)}.br:hover{background:rgba(220,38,38,.06)}
.row{display:flex;gap:7px;margin-top:10px}
.prog{height:2px;background:var(--bdr);border-radius:1px;margin-top:7px;display:none;overflow:hidden}
.prog .fill{height:100%;background:var(--grn);transition:width .2s;width:0}
#st,#fst{font-size:.7rem;font-family:'JetBrains Mono',monospace;color:var(--grn);min-height:1em;margin-top:5px}
table{width:100%;border-collapse:collapse}
th{text-align:left;padding:7px 12px;font-size:.65rem;font-weight:500;color:var(--sub);text-transform:uppercase;letter-spacing:.08em;border-bottom:1px solid var(--bdr);font-family:'JetBrains Mono',monospace;background:var(--s2)}
td{padding:8px 12px;border-bottom:1px solid var(--bdr);vertical-align:middle}
tr:last-child td{border-bottom:none}
tr:hover td{background:var(--hvr)}
.fn{display:flex;align-items:center;gap:7px}
a.nl{color:var(--acc);text-decoration:none;font-size:.86rem;transition:.12s}
a.nl:hover{color:var(--grn)}
.fname{font-size:.86rem;color:var(--txt)}
.sc,.sm{color:var(--sub);font-family:'JetBrains Mono',monospace;font-size:.72rem}
.acts{display:flex;gap:4px;justify-content:flex-end}
.rf{display:none;align-items:center;gap:5px}
.rf input{flex:1;padding:4px 7px;font-size:.76rem}
.empty{text-align:center;padding:40px;color:var(--sub);font-family:'JetBrains Mono',monospace;font-size:.82rem}
@media(max-width:680px){.wrap{flex-direction:column}.side{width:100%}}
</style>
</head>
<body>
<header>
  <span class="logo">sfh<em>.</em></span>
  <span class="badge">manage</span>
  <div class="bc">
    {{range $i,$c:=.Crumbs}}{{if $i}}<span class="s">/</span>{{end}}<a href="{{$c.URL}}">{{$c.Name}}</a>{{end}}
  </div>
  <a class="vw" href="{{.Path}}">👁 view</a>
</header>
<div class="wrap">
<aside class="side">
  <div class="card">
    <h3>Upload Files</h3>
    <div class="dz" id="dz1">
      <span class="ic">📂</span>
      Drop files here<br><small>or click to browse</small>
      <input type="file" id="fi" multiple>
    </div>
    <div class="prog" id="pb"><div class="fill" id="pf"></div></div>
    <div id="st"></div>
  </div>
  <div class="card">
    <h3>Upload Folder</h3>
    <div class="dz" id="dz2">
      <span class="ic">📁</span>
      Drop folder here<br><small>or click to browse</small>
      <input type="file" id="ffi" webkitdirectory multiple>
    </div>
    <div id="fst"></div>
  </div>
  <div class="card">
    <h3>New Folder</h3>
    <form method="POST">
      <input type="hidden" name="action" value="mkdir">
      <input type="text" name="name" placeholder="folder name" required>
      <div class="row"><button class="btn bg" type="submit">＋ Create</button></div>
    </form>
  </div>
</aside>
<main class="main">
  <div class="card">
    <h3>{{.Path}}</h3>
    {{if .Files}}
    <table>
      <thead><tr><th>Name</th><th>Size</th><th>Modified</th><th></th></tr></thead>
      <tbody>
      {{range .Files}}<tr>
        <td>
          <div class="fn" id="fn-{{.Name}}">
            <span>{{if .IsDir}}📁{{else}}📄{{end}}</span>
            {{if .IsDir}}
              <a class="nl" href="/update{{joinDir $.Path .Name}}">{{.Name}}</a>
            {{else}}
              <span class="fname">{{.Name}}</span>
            {{end}}
          </div>
          <div class="rf" id="rf-{{.Name}}">
            <input type="text" id="rn-{{.Name}}" value="{{.Name}}">
            <button class="btn bp" style="padding:3px 8px" onclick="doRename('{{.Name}}')">✓</button>
            <button class="btn bgh" style="padding:3px 8px" onclick="cancelRename('{{.Name}}')">✕</button>
          </div>
        </td>
        <td class="sc">{{.Size}}</td>
        <td class="sm">{{.ModTime}}</td>
        <td><div class="acts">
          {{if not .IsDir}}<a href="{{join $.Path .Name}}" download class="btn bgh" style="padding:3px 8px;font-size:.7rem">⬇</a>{{end}}
          <button class="btn bgh" style="padding:3px 8px;font-size:.7rem" onclick="showRename('{{.Name}}')">✏</button>
          <button class="btn br" style="padding:3px 8px;font-size:.7rem" onclick="doDelete('{{.Name}}')">🗑</button>
        </div></td>
      </tr>{{end}}
      </tbody>
    </table>
    {{else}}<div class="empty">No files yet.</div>{{end}}
  </div>
</main>
</div>

<form id="df" method="POST" style="display:none">
  <input type="hidden" name="action" value="delete">
  <input type="hidden" name="name" id="dn">
</form>
<form id="rf2" method="POST" style="display:none">
  <input type="hidden" name="action" value="rename">
  <input type="hidden" name="old" id="ro">
  <input type="hidden" name="new" id="rn2">
</form>

<script>
const CUR={{js .Path}};
const dz1=document.getElementById('dz1'),fi=document.getElementById('fi');
const pb=document.getElementById('pb'),pf=document.getElementById('pf'),st=document.getElementById('st');
fi.addEventListener('change',()=>uploadFiles(fi.files));
dz1.addEventListener('dragover',e=>{e.preventDefault();dz1.classList.add('ov')});
dz1.addEventListener('dragleave',()=>dz1.classList.remove('ov'));
dz1.addEventListener('drop',e=>{e.preventDefault();dz1.classList.remove('ov');uploadFiles(e.dataTransfer.files)});
function uploadFiles(files){
  if(!files.length)return;
  const fd=new FormData();
  for(const f of files)fd.append('files',f);
  pb.style.display='block';st.textContent='Uploading...';
  const xhr=new XMLHttpRequest();
  xhr.open('POST','/api/upload?path='+encodeURIComponent(CUR));
  xhr.upload.onprogress=e=>{if(e.lengthComputable)pf.style.width=(e.loaded/e.total*100)+'%'};
  xhr.onload=()=>{pb.style.display='none';st.textContent='✓ Done';setTimeout(()=>location.reload(),500)};
  xhr.onerror=()=>{st.textContent='✗ Error'};
  xhr.send(fd);
}
const dz2=document.getElementById('dz2'),ffi=document.getElementById('ffi'),fst=document.getElementById('fst');
ffi.addEventListener('change',()=>uploadFolder(ffi.files));
dz2.addEventListener('dragover',e=>{e.preventDefault();dz2.classList.add('ov')});
dz2.addEventListener('dragleave',()=>dz2.classList.remove('ov'));
dz2.addEventListener('drop',e=>{
  e.preventDefault();dz2.classList.remove('ov');
  const allFiles=[];let pending=0;
  function traverse(entry,path){
    if(entry.isFile){
      pending++;
      entry.file(f=>{allFiles.push(new File([f],path+f.name,{type:f.type}));pending--;if(pending===0)uploadFolder(allFiles)});
    }else if(entry.isDirectory){
      const r=entry.createReader();pending++;
      r.readEntries(es=>{pending--;es.forEach(e2=>traverse(e2,path+entry.name+'/'));if(pending===0&&allFiles.length)uploadFolder(allFiles);});
    }
  }
  for(const item of e.dataTransfer.items){const en=item.webkitGetAsEntry();if(en)traverse(en,'')}
});
async function uploadFolder(files){
  if(!files.length)return;
  fst.textContent='Uploading...';
  const fd=new FormData();
  for(const f of files)fd.append('files',f,f.name);
  try{
    await fetch('/api/upload?path='+encodeURIComponent(CUR),{method:'POST',body:fd});
    fst.textContent='✓ Done';setTimeout(()=>location.reload(),500);
  }catch(e){fst.textContent='✗ '+e.message}
}
function doDelete(name){if(!confirm('Delete "'+name+'"?'))return;document.getElementById('dn').value=name;document.getElementById('df').submit()}
function showRename(name){document.getElementById('fn-'+name).style.display='none';document.getElementById('rf-'+name).style.display='flex'}
function cancelRename(name){document.getElementById('fn-'+name).style.display='flex';document.getElementById('rf-'+name).style.display='none'}
function doRename(old){
  const nw=document.getElementById('rn-'+old).value.trim();
  if(!nw||nw===old){cancelRename(old);return}
  document.getElementById('ro').value=old;document.getElementById('rn2').value=nw;document.getElementById('rf2').submit();
}
</script>
</body></html>"##;

// ─── value model ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum Val {
    S(String),
    B(bool),
    I(i64),
    L(Rc<Vec<Val>>),
    M(Rc<Vec<(String, Val)>>),
    /// template.JS — inserted verbatim (already safe JSON)
    Raw(String),
}

impl Val {
    fn field(&self, name: &str) -> Val {
        match self {
            Val::M(kv) => kv
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(Val::S(String::new())),
            _ => Val::S(String::new()),
        }
    }
    fn truthy(&self) -> bool {
        match self {
            Val::S(s) => !s.is_empty(),
            Val::B(b) => *b,
            Val::I(i) => *i != 0,
            Val::L(l) => !l.is_empty(),
            Val::M(_) => true,
            Val::Raw(_) => true,
        }
    }
    /// The plain-string form used before escaping in HTML/URL contexts.
    fn to_display(&self) -> String {
        match self {
            Val::S(s) => s.clone(),
            Val::B(b) => b.to_string(),
            Val::I(i) => i.to_string(),
            Val::Raw(s) => s.clone(),
            Val::L(_) | Val::M(_) => String::new(),
        }
    }
}

// ─── escaping (Go html/template tables) ──────────────────────────────────────

/// Go htmlReplacementTable (html.go).
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&#34;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '+' => out.push_str("&#43;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\0' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// Go isSafeURL (url.go): only http/https/mailto schemes allowed.
fn is_safe_url(s: &str) -> bool {
    if let Some((protocol, _)) = s.split_once(':') {
        if !protocol.contains('/') {
            let p = protocol.to_ascii_lowercase();
            if p != "http" && p != "https" && p != "mailto" {
                return false;
            }
        }
    }
    true
}

/// Go urlNormalizer (url.go processURLOnto with norm=true), lowercase hex.
fn url_normalize(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                out.push(c as char);
                i += 1;
            }
            b'!' | b'#' | b'$' | b'&' | b'*' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?'
            | b'@' | b'[' | b']' => {
                out.push(c as char);
                i += 1;
            }
            b'-' | b'.' | b'_' | b'~' => {
                out.push(c as char);
                i += 1;
            }
            b'%' => {
                if i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
                    out.push('%');
                } else {
                    out.push_str("%25");
                }
                i += 1;
            }
            _ => {
                out.push_str(&format!("%{:02x}", c));
                i += 1;
            }
        }
    }
    out
}

fn url_escape_attr(s: &str) -> String {
    // urlFilter → urlNormalizer, then htmlEscaper (attribute context).
    if !is_safe_url(s) {
        return html_escape("#ZgotmplZ");
    }
    html_escape(&url_normalize(s))
}

/// Go jsStrReplacementTable (js.go).
fn jsstr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push_str("\\u0000"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0b}' => out.push_str("\\u000b"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\u0022"),
            '`' => out.push_str("\\u0060"),
            '&' => out.push_str("\\u0026"),
            '\'' => out.push_str("\\u0027"),
            '+' => out.push_str("\\u002b"),
            '/' => out.push_str("\\/"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

// ─── AST ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ctx {
    Html,
    Url,
    JsStr,
    Raw, // inside <script>: {{js ...}} inserted verbatim
}

#[derive(Clone)]
enum Node {
    Text(String),
    Action(Expr, Ctx),
    If(Expr, Vec<Node>, Vec<Node>),
    Range {
        idx_var: Option<String>,
        val_var: Option<String>,
        expr: Expr,
        body: Vec<Node>,
        else_body: Vec<Node>,
    },
}

#[derive(Clone)]
enum Expr {
    Field(Vec<String>),           // .A.B or $c.URL → path segments
    Root(Vec<String>),            // $.Path
    Var(String, Vec<String>),     // $i / $c
    Not(Box<Expr>),
    Len(Box<Expr>),
    Join(Box<Expr>, Box<Expr>, bool), // (base, name, is_dir)
    Js(Box<Expr>),
}

fn parse_expr(s: &str) -> Expr {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("not ") {
        return Expr::Not(Box::new(parse_expr(rest)));
    }
    if let Some(rest) = s.strip_prefix("len ") {
        return Expr::Len(Box::new(parse_expr(rest)));
    }
    if let Some(rest) = s.strip_prefix("js ") {
        return Expr::Js(Box::new(parse_expr(rest)));
    }
    for func in ["joinDir", "join"] {
        if let Some(rest) = s.strip_prefix(func) {
            let rest = rest.trim_start();
            // Go template calls are written without parens: {{join $.Path .Name}}
            let args_str = if let Some(inner) = rest.strip_prefix('(') {
                inner.strip_suffix(')').unwrap_or(inner)
            } else {
                rest
            };
            if let Some((a, b)) = split_args(args_str) {
                return Expr::Join(
                    Box::new(parse_expr(&a)),
                    Box::new(parse_expr(&b)),
                    func == "joinDir",
                );
            }
        }
    }
    // dotted access: $.Path / $c.URL / .Path / $i
    let (head, mut rest) = split_head(s);
    let base: Expr = if head.starts_with('$') {
        let name = &head[1..];
        // rest like ".URL" or ".A.B"; drop the empty leading segment
        let segs: Vec<String> = rest
            .split('.')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect();
        if name.is_empty() {
            Expr::Root(segs)
        } else {
            Expr::Var(name.to_string(), segs)
        }
    } else if head.starts_with('.') {
        // .Field or plain .
        let mut segs: Vec<String> = Vec::new();
        segs.push(head[1..].to_string());
        while let Some((a, b)) = rest.split_once('.') {
            segs.push(a.to_string());
            rest = b.to_string();
        }
        if !rest.is_empty() {
            segs.push(rest.to_string());
        }
        let segs2: Vec<String> = segs.into_iter().filter(|x| !x.is_empty()).collect();
        if segs2.is_empty() {
            Expr::Var(".".to_string(), vec![])
        } else {
            Expr::Field(segs2)
        }
    } else {
        Expr::Var(head.to_string(), vec![])
    };
    base
}

fn split_head(s: &str) -> (String, String) {
    // returns head token and remainder (starting with '.' if any);
    // a leading dot is field syntax, not a separator (".Path" → head ".Path")
    for (i, c) in s.char_indices() {
        if i == 0 {
            continue;
        }
        if c == '.' {
            return (s[..i].to_string(), s[i..].to_string());
        }
    }
    (s.to_string(), String::new())
}

fn split_args(s: &str) -> Option<(String, String)> {
    // split top-level args on whitespace respecting parentheses
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 && c.is_whitespace() => {
                let (a, b) = (&s[..i], &s[i..]);
                return Some((a.to_string(), b.trim().to_string()));
            }
            _ => {}
        }
    }
    None
}

// ─── parser ──────────────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

enum Tok {
    Text(String),
    Cmd(String),
}

fn tokenize(tmpl: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut rest = tmpl;
    while let Some(start) = rest.find("{{") {
        if start > 0 {
            toks.push(Tok::Text(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            toks.push(Tok::Text(rest[start..].to_string()));
            rest = "";
            break;
        };
        toks.push(Tok::Cmd(after[..end].trim().to_string()));
        rest = &after[end + 2..];
    }
    if !rest.is_empty() {
        toks.push(Tok::Text(rest.to_string()));
    }
    toks
}

impl Parser {
    /// Parse until {{else}} / {{end}} of the current nesting level.
    /// Returns (nodes, terminator).
    fn parse_until(&mut self) -> (Vec<Node>, String) {
        let mut out: Vec<Node> = Vec::new();
        loop {
            let Some(tok) = self.toks.get(self.pos) else {
                return (out, "eof".to_string());
            };
            match tok {
                Tok::Text(t) => {
                    out.push(Node::Text(t.clone()));
                    self.pos += 1;
                }
                Tok::Cmd(c) => {
                    let cmd = c.clone();
                    self.pos += 1;
                    if cmd == "end" || cmd == "else" {
                        return (out, cmd);
                    }
                    if let Some(expr) = cmd.strip_prefix("if ") {
                        let (body, term) = self.parse_until();
                        let mut else_body = Vec::new();
                        if term == "else" {
                            let (eb, term2) = self.parse_until();
                            else_body = eb;
                            let _ = term2; // "end"
                        }
                        out.push(Node::If(parse_expr(expr), body, else_body));
                        continue;
                    }
                    if let Some(expr) = cmd.strip_prefix("range ") {
                        // forms: "range $i,$c:=.X" | "range .X"
                        let (idx_var, val_var, expr) = parse_range_header(expr);
                        let (body, term) = self.parse_until();
                        let mut else_body = Vec::new();
                        if term == "else" {
                            let (eb, _) = self.parse_until();
                            else_body = eb;
                        }
                        out.push(Node::Range {
                            idx_var,
                            val_var,
                            expr: parse_expr(&expr),
                            body,
                            else_body,
                        });
                        continue;
                    }
                    // plain action — determine escaping context from the
                    // preceding literal text (mirrors html/template's lexer).
                    let ctx = ctx_from_tail(&out);
                    out.push(Node::Action(parse_expr(&cmd), ctx));
                }
            }
        }
    }
}

fn ctx_from_tail(out: &[Node]) -> Ctx {
    // find trailing literal text
    let mut tail = String::new();
    for n in out.iter().rev() {
        if let Node::Text(t) = n {
            tail = t.clone();
        } else {
            break;
        }
    }
    // inside <script> after the last <script> tag?
    let last_open = tail.rfind("<script>");
    let last_close = tail.rfind("</script>");
    let in_script = match (last_open, last_close) {
        (Some(o), Some(c)) => o > c,
        (Some(_), None) => true,
        _ => false,
    };
    if in_script {
        return Ctx::Raw;
    }
    // inside an unclosed URL attribute? (href="/update{{...}}" — the action
    // is part of the URL value even when text precedes it in the attribute)
    for attr in ["href=\"", "src=\"", "action=\""] {
        if let Some(pos) = tail.rfind(attr) {
            if !tail[pos + attr.len()..].contains('"') {
                return Ctx::Url;
            }
        }
    }
    if tail.ends_with("('") {
        return Ctx::JsStr;
    }
    Ctx::Html
}

fn parse_range_header(s: &str) -> (Option<String>, Option<String>, String) {
    let s = s.trim();
    if let Some(eq) = s.find(":=") {
        let vars = &s[..eq];
        let expr = s[eq + 2..].trim().to_string();
        if let Some((i, v)) = vars.split_once(',') {
            return (
                Some(i.trim().trim_start_matches('$').to_string()),
                Some(v.trim().trim_start_matches('$').to_string()),
                expr,
            );
        }
        return (None, Some(vars.trim().trim_start_matches('$').to_string()), expr);
    }
    (None, None, s.to_string())
}

fn parse_template(tmpl: &str) -> Vec<Node> {
    let mut p = Parser {
        toks: tokenize(tmpl),
        pos: 0,
    };
    let (nodes, _) = p.parse_until();
    nodes
}

// ─── eval ────────────────────────────────────────────────────────────────────

fn eval(expr: &Expr, root: &Val, cur: &Val, vars: &[(String, Val)]) -> Val {
    match expr {
        Expr::Field(segs) => {
            let mut v = cur.clone();
            for s in segs {
                v = v.field(s);
            }
            v
        }
        Expr::Root(segs) => {
            let mut v = root.clone();
            for s in segs {
                v = v.field(s);
            }
            v
        }
        Expr::Var(name, segs) => {
            if name == "." {
                let mut v = cur.clone();
                for s in segs {
                    v = v.field(s);
                }
                return v;
            }
            let base = vars
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(Val::S(String::new()));
            let mut v = base;
            for s in segs {
                v = v.field(s);
            }
            v
        }
        Expr::Not(e) => Val::B(!eval(e, root, cur, vars).truthy()),
        Expr::Len(e) => match eval(e, root, cur, vars) {
            Val::L(l) => Val::I(l.len() as i64),
            Val::S(s) => Val::I(s.len() as i64),
            Val::M(m) => Val::I(m.len() as i64),
            _ => Val::I(0),
        },
        Expr::Join(base, name, is_dir) => {
            let b = eval(base, root, cur, vars).to_display();
            let n = eval(name, root, cur, vars).to_display();
            let trimmed = b.strip_suffix('/').unwrap_or(&b).to_string();
            let joined = if *is_dir {
                format!("{trimmed}/{n}/")
            } else {
                format!("{trimmed}/{n}")
            };
            Val::S(joined)
        }
        Expr::Js(e) => {
            let v = eval(e, root, cur, vars);
            Val::Raw(crate::util::gojson_string(v.to_display().as_bytes()))
        }
    }
}

fn render_nodes(nodes: &[Node], root: &Val, cur: &Val, vars: &[(String, Val)], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Action(e, ctx) => {
                let v = eval(e, root, cur, vars);
                match v {
                    Val::Raw(s) => out.push_str(&s),
                    v => {
                        let d = v.to_display();
                        match ctx {
                            Ctx::Html => out.push_str(&html_escape(&d)),
                            Ctx::Url => out.push_str(&url_escape_attr(&d)),
                            Ctx::JsStr => out.push_str(&html_escape(&jsstr_escape(&d))),
                            Ctx::Raw => out.push_str(&d),
                        }
                    }
                }
            }
            Node::If(e, body, else_body) => {
                if eval(e, root, cur, vars).truthy() {
                    render_nodes(body, root, cur, vars, out);
                } else {
                    render_nodes(else_body, root, cur, vars, out);
                }
            }
            Node::Range {
                idx_var,
                val_var,
                expr,
                body,
                else_body,
            } => {
                let v = eval(expr, root, cur, vars);
                let items: Vec<Val> = match v {
                    Val::L(l) => l.as_ref().clone(),
                    Val::M(_) => Vec::new(), // not used by these templates
                    _ => Vec::new(),
                };
                if items.is_empty() {
                    render_nodes(else_body, root, cur, vars, out);
                    continue;
                }
                for (i, item) in items.into_iter().enumerate() {
                    let mut new_vars: Vec<(String, Val)> = vars.to_vec();
                    if let Some(iv) = idx_var {
                        new_vars.push((iv.clone(), Val::I(i as i64)));
                    }
                    if let Some(vv) = val_var {
                        new_vars.push((vv.clone(), item.clone()));
                    }
                    // inside range, "." is the element when no val_var named capture
                    let new_cur = if val_var.is_some() { cur.clone() } else { item };
                    // NOTE: for `range $i,$c:=.Crumbs`, "." stays the outer value;
                    // for `range .Files`, "." is the element.
                    render_nodes(body, root, &new_cur, &new_vars, out);
                }
            }
        }
    }
}

pub fn render(tmpl: &str, data: &Val) -> String {
    let nodes = parse_template(tmpl);
    let mut out = String::with_capacity(tmpl.len() + 256);
    render_nodes(&nodes, data, data, &[], &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: Vec<(&str, &str)>) -> Val {
        Val::M(Rc::new(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), Val::S(v.to_string())))
                .collect(),
        ))
    }

    #[test]
    fn range_with_vars_and_fields() {
        let data = Val::M(Rc::new(vec![(
            "Crumbs".to_string(),
            Val::L(Rc::new(vec![m(vec![("Name", "~"), ("URL", "/update")])])),
        )]));
        let out = render(
            "{{range $i,$c:=.Crumbs}}[{{if $i}}S{{end}}<a href=\"{{$c.URL}}\">{{$c.Name}}</a>]{{end}}",
            &data,
        );
        assert_eq!(out, r#"[<a href="/update">~</a>]"#, "got: {out}");
    }

    #[test]
    fn dot_path_field() {
        let data = m(vec![("Path", "/")]);
        // eval in isolation
        let e = parse_expr(".Path");
        let v = eval(&e, &data, &data, &[]);
        assert_eq!(v.to_display(), "/", "eval(.Path) failed");
        // parse node identity: Field should have 1 segment
        if let Expr::Field(segs) = &e {
            assert_eq!(segs, &["Path".to_string()], "wrong segments");
        } else {
            panic!("parse_expr(.Path) did not produce Field");
        }
        assert_eq!(render("<title>{{.Path}}</title>", &data), "<title>/</title>");
        assert_eq!(render("<title>{{$.Path}}</title>", &data), "<title>/</title>");
    }
}
