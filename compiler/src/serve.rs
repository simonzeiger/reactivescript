//! `rxc serve`'s requests (spec/11 CP-8e), for a language server: the native binary answers them a line at a time, the
//! WebAssembly build (`wasm.rs`) a call at a time, the same way. `disk` reads a file the editor does not have open.
use crate::format::{self, DiagnosticView};
use serde_json::{json, Value};
use std::collections::HashMap;

/// One request: `analyze` (`{file, overlays, prelude}`) or `prelude`.
pub fn request(method: &str, params: &Value, disk: &dyn Fn(&str) -> Option<String>) -> Value {
    match method {
        "analyze" => analyze(params, disk),
        "prelude" => json!(crate::project::PRELUDE),
        _ => json!({ "error": "unknown method" }),
    }
}

/// `rxc check --json`: a diagnostic with its range (CP-8), for an editor or an agent. Lines are 1-based, columns 0-based in
/// UTF-16 code units, as LSP counts; `start`/`end` are byte offsets; `rule` names the spec rule (spec/99).
pub fn diagnostic_json(set: &crate::modules::ModuleSet, d: &crate::check::Diagnostic) -> Value {
    diagnostic_json_with(set, d, false)
}

pub fn diagnostic_json_rendered(set: &crate::modules::ModuleSet, d: &crate::check::Diagnostic) -> Value {
    diagnostic_json_with(set, d, true)
}

fn line_end_u16(src: &str, line_1based: usize) -> usize {
    src.lines().nth(line_1based.saturating_sub(1)).map(|l| l.trim_end().chars().map(char::len_utf16).sum()).unwrap_or(1)
}

fn cols_for_range(src: &str, l0: usize, c0: usize, l1: usize, c1: usize) -> (usize, usize) {
    if l0 == l1 { (c0, c1) } else { (c0, line_end_u16(src, l0).max(c0 + 1)) }
}

fn checker_span(set: &crate::modules::ModuleSet, file: &str, line: usize) -> (Option<(usize, usize)>, Option<Value>) {
    let (Some(src), Some(p)) = (set.sources.get(file).map(|s| s.as_str()), set.programs.get(file)) else { return (None, None) };
    let Some(r) = crate::spans::line_range(src, p, line) else { return (None, None) };
    let ((l0, c0), (l1, c1)) = (crate::spans::line_col(src, r.start), crate::spans::line_col(src, r.end));
    let range = json!({ "start": { "line": l0, "column": c0 }, "end": { "line": l1, "column": c1 }, "offsets": [r.start, r.end] });
    (Some(cols_for_range(src, l0, c0, l1, c1)), Some(range))
}

pub fn format_checker_diagnostic(set: &crate::modules::ModuleSet, d: &crate::check::Diagnostic, display_file: &str, compact: bool, color: bool) -> String {
    let file = d.file.as_deref().unwrap_or(&set.entry);
    let line = d.line.max(1) as usize;
    let (cols, _) = checker_span(set, file, line);
    let view = DiagnosticView { file: display_file, line, cols, code: &d.code, rule: &d.rule, msg: &d.msg, source: set.sources.get(file).map(|s| s.as_str()) };
    format::format_diagnostic(&view, compact, color)
}

fn diagnostic_json_with(set: &crate::modules::ModuleSet, d: &crate::check::Diagnostic, with_rendered: bool) -> Value {
    let file = d.file.clone().unwrap_or_else(|| set.entry.clone());
    let line = d.line.max(1) as usize;
    let parts = format::split_message(&d.msg);
    let (cols, range) = checker_span(set, &file, line);
    let mut v = json!({
        "file": file,
        "line": d.line,
        "code": d.code,
        "rule": d.rule,
        "message": d.msg,
        "head": parts.head,
        "help": parts.help,
        "notes": parts.notes,
    });
    if let Some(r) = range {
        v["range"] = r;
    }
    if with_rendered {
        let view = DiagnosticView { file: &file, line, cols, code: &d.code, rule: &d.rule, msg: &d.msg, source: set.sources.get(&file).map(|s| s.as_str()) };
        v["rendered"] = json!(format::format_diagnostic(&view, false, false));
    }
    v
}

/// A parse error's message without the `file: line n:` (or `.d.rx` `file:n:`) prefix it carries.
pub fn parse_message(e: &crate::parse::ParseError) -> &str {
    static LINE_PREFIX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^(?:.+?: )?line \d+: ").unwrap());
    static SIG_PREFIX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^.+?:\d+: ").unwrap());
    if let Some(m) = LINE_PREFIX.find(&e.0) {
        return &e.0[m.end()..];
    }
    if let Some(m) = SIG_PREFIX.find(&e.0) {
        return &e.0[m.end()..];
    }
    &e.0
}

fn parse_span(e: &crate::parse::ParseError, src: Option<&str>) -> (Option<usize>, Option<(usize, usize)>, Option<Value>) {
    static SIG_LINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^.+?:(\d+): ").unwrap());
    if let (Some(at), Some(src)) = (e.1, src) {
        let at = at.min(src.len());
        let end = src[at..].find('\n').map(|j| at + j).unwrap_or(src.len());
        let end = at + src[at..end].trim_end().len();
        let ((l0, c0), (l1, c1)) = (crate::spans::line_col(src, at as u32), crate::spans::line_col(src, end as u32));
        let range = json!({ "start": { "line": l0, "column": c0 }, "end": { "line": l1, "column": c1 }, "offsets": [at, end] });
        return (Some(l0.max(1)), Some(cols_for_range(src, l0, c0, l1, c1)), Some(range));
    }
    if let Some(ln) = SIG_LINE.captures(&e.0).and_then(|c| c[1].parse::<usize>().ok()) {
        let line = ln.max(1);
        if let Some(src) = src {
            let mut byte_pos = 0usize;
            for (idx, raw_line) in src.split('\n').enumerate() {
                if idx + 1 == line {
                    let clean = raw_line.strip_suffix('\r').unwrap_or(raw_line);
                    let start = byte_pos + (clean.len() - clean.trim_start().len());
                    let end = (byte_pos + clean.trim_end().len()).max(start);
                    let ((l0, c0), (l1, c1)) = (crate::spans::line_col(src, start as u32), crate::spans::line_col(src, end as u32));
                    let range = json!({ "start": { "line": l0, "column": c0 }, "end": { "line": l1, "column": c1 }, "offsets": [start, end] });
                    return (Some(line), Some((c0, c1)), Some(range));
                }
                byte_pos += raw_line.len() + 1;
            }
        }
        return (Some(line), None, None);
    }
    (None, None, None)
}

pub fn format_parse_error(display_file: &str, e: &crate::parse::ParseError, src: Option<&str>, compact: bool, color: bool) -> String {
    let msg = parse_message(e);
    let rule = format::extract_spec_rule(msg).unwrap_or_else(|| "07".into());
    let (line, cols, _) = parse_span(e, src);
    let view = DiagnosticView { file: display_file, line: line.unwrap_or(1), cols, code: e.3, rule: &rule, msg, source: src };
    format::format_diagnostic(&view, compact, color)
}

pub fn parse_error_in(file: &str, e: &crate::parse::ParseError, src: Option<&str>) -> Value {
    parse_error_in_with(file, e, src, false)
}

pub fn parse_error_in_rendered(file: &str, e: &crate::parse::ParseError, src: Option<&str>) -> Value {
    parse_error_in_with(file, e, src, true)
}

fn parse_error_in_with(file: &str, e: &crate::parse::ParseError, src: Option<&str>, with_rendered: bool) -> Value {
    let msg = parse_message(e);
    let rule = format::extract_spec_rule(msg).unwrap_or_else(|| "07".into());
    let parts = format::split_message(msg);
    let (line_opt, cols, range) = parse_span(e, src);
    let mut v = json!({
        "file": file,
        "code": e.3,
        "rule": rule,
        "message": msg,
        "head": parts.head,
        "help": parts.help,
        "notes": parts.notes,
    });
    if let Some(ln) = line_opt {
        v["line"] = json!(ln);
    }
    if let Some(r) = range {
        v["range"] = r;
    }
    if with_rendered {
        let view = DiagnosticView { file, line: line_opt.unwrap_or(1), cols, code: e.3, rule: &rule, msg, source: src };
        v["rendered"] = json!(format::format_diagnostic(&view, false, false));
    }
    v
}

/// UTF-16 offsets of a text's byte offsets (what JavaScript strings, Volar and LSP index by).
pub fn utf16_index(s: &str) -> Vec<u32> {
    let mut idx = vec![0u32; s.len() + 1];
    let mut u = 0u32;
    for (b, ch) in s.char_indices() {
        for k in 0..ch.len_utf8() {
            idx[b + k] = u;
        }
        u += ch.len_utf16() as u32;
    }
    idx[s.len()] = u;
    idx
}

/// `rxc serve`, one request: `{file, overlays: {path: text}, prelude}` — the file loaded (and its imports) as an editor
/// has them, recovering (CP-8e); its projection with mappings in UTF-16 offsets, and its diagnostics: the parse's, else
/// the checker's.
pub fn analyze(params: &Value, disk: &dyn Fn(&str) -> Option<String>) -> Value {
    let file = params["file"].as_str().unwrap_or_default().to_string();
    let overlays: HashMap<String, String> = params["overlays"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()).unwrap_or_default();
    let read = |p: &str| overlays.get(p).cloned().or_else(|| disk(p));
    let set = match crate::modules::load_modules_recovering(&file, &read) {
        Ok(s) => s,
        Err(e) => {
            let src = e.2.as_deref().and_then(|f| read(f));
            return json!({ "diagnostics": [parse_error_in_rendered(e.2.as_deref().unwrap_or(&file), &e, src.as_deref())] });
        }
    };
    let src = set.sources.get(&file).cloned().unwrap_or_default();
    let prelude = params["prelude"].as_str();
    let mut out = json!({ "code": null, "mappings": [] });
    if let Some(p) = crate::project::project_modules_with(&set, prelude).into_iter().find(|p| p.rx == file) {
        let (gi, si) = (utf16_index(&p.code), utf16_index(&src));
        let maps: Vec<[u32; 3]> = p.mappings.iter().map(|m| [gi[m.gen as usize], si[m.src as usize], si[(m.src + m.len) as usize] - si[m.src as usize]]).collect();
        let u = |i: &[u32], a: u32, n: u32| [i[a as usize], i[(a + n) as usize] - i[a as usize]];
        let ranges: Vec<[u32; 4]> = p.ranges.iter().map(|r| { let (g, s) = (u(&gi, r.gen, r.glen), u(&si, r.src, r.len)); [g[0], g[1], s[0], s[1]] }).collect();
        out = json!({ "code": p.code, "mappings": maps, "ranges": ranges });
    }
    let own: Vec<Value> = if set.parse_errors.iter().any(|e| e.2.as_deref() == Some(file.as_str())) {
        set.parse_errors.iter().filter(|e| e.2.as_deref() == Some(file.as_str())).map(|e| parse_error_in_rendered(&file, e, Some(&src))).collect()
    } else {
        crate::check::check_modules(&set, params["spa"].as_bool().unwrap_or(false)).diags.iter().map(|d| diagnostic_json_rendered(&set, d)).filter(|d| d["file"] == json!(file)).collect()
    };
    out["diagnostics"] = json!(own);
    out
}
