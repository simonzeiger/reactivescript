//! Diagnostic splitting and formatting (`rxc`, Vite, `rx-tsc`, and LSP).
//!
//! - Non-TTY pipes (agents, CI, subprocesses): compact 1-line `file:line:col: error CODE [rule] message` by default.
//! - Interactive TTYs (or `--pretty` / `RX_PRETTY=1`): rustc-style block with source line, `^^^` carets, `= help:`, and `= note:`.

use regex::Regex;
use std::cell::Cell;
use std::sync::LazyLock;

pub const GUIDE_BASE: &str = "https://github.com/solidjs/reactivescript/blob/main/guide/diagnostics.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDiagnostic {
    pub head: String,
    pub help: Vec<String>,
    pub notes: Vec<String>,
}

const SPEC_RULE_ATOM: &str = r"[A-Z]{1,4}-(?:f-|io-)?\d+[a-z]?";

static SPEC_RULE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"\b({SPEC_RULE_ATOM})\b")).unwrap());
static RULE_CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^\(({SPEC_RULE_ATOM}(?:,\s*{SPEC_RULE_ATOM})*)\)")).unwrap());
static RULE_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^({SPEC_RULE_ATOM}):\s+(.*)$")).unwrap());

fn is_spec_rule_atom(s: &str) -> bool {
    !s.starts_with("UTF-") && SPEC_RULE_RE.find(s).is_some_and(|m| m.start() == 0 && m.end() == s.len())
}

/// Extract a valid spec rule ID from a parse error message (after stripping any `near '<user code>'` slice).
pub fn extract_spec_rule(msg: &str) -> Option<String> {
    let cleaned = strip_near_clause(msg);
    SPEC_RULE_RE.captures_iter(&cleaned).map(|c| c[1].to_string()).find(|r| !r.starts_with("UTF-"))
}

/// Strip the parser's raw 20-char source excerpt (` near '<slice>'`) before structural splitting or rule extraction.
pub fn strip_near_clause(s: &str) -> String {
    let Some(near_idx) = s.find(" near '") else { return s.to_string() };
    let after_open = &s[near_idx + " near '".len()..];
    if let Some(without_close) = after_open.strip_suffix('\'') {
        if !without_close.contains('\n') && without_close.chars().count() <= 20 {
            return s[..near_idx].to_string();
        }
    }
    let mut best_close = None;
    for (char_count, (byte_off, ch)) in after_open.char_indices().enumerate() {
        if ch == '\n' || char_count > 20 {
            break;
        }
        if ch == '\'' && after_open[byte_off + 1..].starts_with(" — ") {
            best_close = Some(byte_off);
        }
    }
    match best_close {
        Some(close_off) => format!("{}{}", &s[..near_idx], &after_open[close_off + 1..]),
        None => s.to_string(),
    }
}

/// Collapse nested `` `…` `` placeholders inside an outer backtick code span (e.g. `` `await `…`` `` -> `` `await '…'` ``).
fn normalize_nested_backticks(s: &str) -> String {
    if !s.contains("`…`") {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut out, mut in_bt, mut i) = (String::with_capacity(s.len()), false, 0usize);
    while i < chars.len() {
        if in_bt && i + 3 < chars.len() && chars[i..i + 3] == ['`', '…', '`'] {
            if chars[i + 3..].iter().filter(|&&c| c == '`').count() % 2 == 1 {
                out.push_str("'…'");
                i += 3;
                continue;
            }
        }
        if chars[i] == '`' {
            in_bt = !in_bt;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Split `s` on `sep` only at top level — never inside backticks, `"..."`, `(...)`, or `{...}`.
fn scan_top(s: &str, sep: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut start, mut bt, mut dq, mut p, mut b) = (0usize, false, false, 0usize, 0usize);
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut idx = 0usize;
    while idx < chars.len() {
        let (byte_i, c) = chars[idx];
        let prev_c = idx.checked_sub(1).and_then(|j| chars.get(j)).map(|x| x.1);
        if !dq && c == '`' {
            bt = !bt;
            idx += 1;
            continue;
        }
        if bt {
            idx += 1;
            continue;
        }
        if c == '"' && prev_c != Some('\\') {
            dq = !dq;
            idx += 1;
            continue;
        }
        if dq {
            idx += 1;
            continue;
        }
        if p == 0 && b == 0 && s[byte_i..].starts_with(sep) {
            parts.push(s[start..byte_i].to_string());
            start = byte_i + sep.len();
            while idx < chars.len() && chars[idx].0 < start {
                idx += 1;
            }
            continue;
        }
        match c {
            '(' => p += 1,
            ')' if p > 0 => p -= 1,
            '{' => b += 1,
            '}' if b > 0 => b -= 1,
            _ => {}
        }
        idx += 1;
    }
    parts.push(s[start..].to_string());
    parts
}

fn peel_trailing_notes(tail: &str) -> (String, Vec<String>) {
    let sentences = scan_top(tail, ". ");
    if sentences.len() <= 1 {
        return (tail.to_string(), vec![]);
    }
    let mut split_at = sentences.len();
    while split_at > 1 && sentences[split_at - 1].trim().chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        split_at -= 1;
    }
    if split_at == sentences.len() {
        return (tail.to_string(), vec![]);
    }
    let notes = sentences[split_at..].iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    (sentences[..split_at].join(". "), notes)
}

fn is_note_clause(tail: &str) -> bool {
    let t = tail.trim();
    t.starts_with("its ") || t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn split_bullets(tail: &str) -> Vec<String> {
    let semi: Vec<String> = scan_top(tail, "; ").into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    if semi.len() >= 3 || (semi.len() == 2 && semi[1].starts_with("or ")) {
        return semi;
    }
    let or_parts: Vec<String> = scan_top(tail, ", or ").into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    if or_parts.len() > 1 {
        let mut out = vec![or_parts[0].clone()];
        for x in &or_parts[1..] {
            out.push(if x.starts_with("or ") { x.clone() } else { format!("or {x}") });
        }
        return out;
    }
    let single = tail.trim();
    if single.is_empty() { vec![] } else { vec![single.to_string()] }
}

/// Split a diagnostic message on top-level punctuation into `head`, `help` bullets, and `notes`.
pub fn split_message(raw_msg: &str) -> SplitDiagnostic {
    let mut text = normalize_nested_backticks(&strip_near_clause(raw_msg.trim()));
    if let Some(caps) = RULE_PREFIX.captures(&text) {
        if is_spec_rule_atom(&caps[1]) {
            text = caps[2].to_string();
        }
    }

    let dashes = scan_top(&text, " — ");
    if dashes.len() >= 2 && dashes.len() % 2 == 0 {
        let head = dashes[..dashes.len() - 1].join(" — ").trim().to_string();
        let raw_tail = dashes.last().unwrap().trim();
        if is_note_clause(raw_tail) {
            let notes = if raw_tail.is_empty() { vec![] } else { vec![raw_tail.to_string()] };
            return SplitDiagnostic { head, help: vec![], notes };
        }
        let (main_tail, notes) = peel_trailing_notes(raw_tail);
        return SplitDiagnostic { head, help: split_bullets(main_tail.trim()), notes };
    }

    let semi_probe = scan_top(&text, "; ");
    if semi_probe.len() >= 2 && semi_probe.last().is_some_and(|s| s.trim_start().starts_with("or ")) {
        let cols = scan_top(&semi_probe[0], ": ");
        if cols.len() >= 2 {
            let head = cols[..cols.len() - 1].join(": ").trim().to_string();
            let mut help = vec![cols.last().unwrap().trim().to_string()];
            help.extend(semi_probe[1..].iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()));
            if help.len() > 1 {
                return SplitDiagnostic { head, help, notes: vec![] };
            }
        }
    }

    SplitDiagnostic { head: text, help: vec![], notes: vec![] }
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RED: &str = "\x1b[1;31m";
const ANSI_BLUE: &str = "\x1b[36m";
const ANSI_GUTTER: &str = "\x1b[2;36m";

fn is_valid_citation(span: &str) -> bool {
    span.trim_start_matches('(').trim_end_matches(')').split(',').all(|p| is_spec_rule_atom(p.trim()))
}

fn style_prose_ansi(text: &str, base: &str, in_code: &Cell<bool>) -> String {
    let mut code = in_code.get();
    let mut out = if code { format!("{ANSI_RESET}{ANSI_BLUE}") } else { String::new() };
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with('`') {
            if code {
                out.push_str(&format!("`{ANSI_RESET}{base}"));
                code = false;
            } else {
                out.push_str(&format!("{ANSI_RESET}{ANSI_BLUE}`"));
                code = true;
            }
            rest = &rest[1..];
            continue;
        }
        if !code && rest.starts_with('(') {
            if let Some(m) = RULE_CITATION.find(rest) {
                let span = m.as_str();
                if is_valid_citation(span) {
                    out.push_str(&format!("{ANSI_RESET}{ANSI_DIM}{span}{ANSI_RESET}{base}"));
                    rest = &rest[span.len()..];
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    if code {
        out.push_str(ANSI_RESET);
    }
    in_code.set(code);
    if base.is_empty() { out } else { format!("{base}{out}{ANSI_RESET}") }
}

/// Terminal / monospace cell width of `ch` (tabs expand to 2 spaces; combining marks/ZWJ/VS are 0; wide CJK & emoji are 2).
fn char_display_width(ch: char) -> usize {
    match ch {
        '\t' => 2,
        '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{200B}'..='\u{200D}'
        | '\u{20D0}'..='\u{20FF}' | '\u{FE00}'..='\u{FE0F}' | '\u{FE20}'..='\u{FE2F}' | '\u{E0100}'..='\u{E01EF}' => 0,
        '\u{1100}'..='\u{115F}' | '\u{231A}'..='\u{231B}' | '\u{23E9}'..='\u{23F3}' | '\u{23F8}'..='\u{23FA}'
        | '\u{25FD}'..='\u{25FE}' | '\u{2614}'..='\u{2615}' | '\u{2648}'..='\u{2653}' | '\u{267F}' | '\u{2693}'
        | '\u{26A1}' | '\u{26AA}'..='\u{26AB}' | '\u{26BD}'..='\u{26BE}' | '\u{26C4}'..='\u{26C5}' | '\u{26CE}'
        | '\u{26D4}' | '\u{26EA}' | '\u{26F2}'..='\u{26F3}' | '\u{26F5}' | '\u{26FA}' | '\u{26FD}' | '\u{2705}'
        | '\u{270A}'..='\u{270B}' | '\u{2728}' | '\u{274C}' | '\u{274E}' | '\u{2753}'..='\u{2755}' | '\u{2757}'
        | '\u{2795}'..='\u{2797}' | '\u{27B0}' | '\u{27BF}' | '\u{2B1B}'..='\u{2B1C}' | '\u{2B50}' | '\u{2B55}'
        | '\u{2E80}'..='\u{303E}' | '\u{3040}'..='\u{A4CF}' | '\u{AC00}'..='\u{D7A3}' | '\u{F900}'..='\u{FAFF}'
        | '\u{FE10}'..='\u{FE19}' | '\u{FE30}'..='\u{FE6F}' | '\u{FF01}'..='\u{FF60}' | '\u{FFE0}'..='\u{FFE6}'
        | '\u{1F004}' | '\u{1F0CF}' | '\u{1F18E}' | '\u{1F1E6}'..='\u{1F251}' | '\u{1F300}'..='\u{1F64F}'
        | '\u{1F680}'..='\u{1F6FF}' | '\u{1F900}'..='\u{1FAFF}' | '\u{20000}'..='\u{3FFFD}' => 2,
        _ => 1,
    }
}

fn str_display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

fn wrap_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let mut tok = String::new();
        while !rest.is_empty() {
            let ch = rest.chars().next().unwrap();
            if ch.is_whitespace() {
                break;
            }
            if ch == '`' {
                if let Some(end) = rest[1..].find('`') {
                    let span = &rest[..end + 2];
                    if str_display_width(span) <= 72 {
                        tok.push_str(span);
                        rest = &rest[end + 2..];
                        continue;
                    }
                }
            }
            tok.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
        if !tok.is_empty() {
            tokens.push(tok);
        }
    }
    tokens
}

fn wrap_with_prefix<F: Fn(&str) -> String>(
    text: &str,
    first_prefix: &str,
    rest_prefix: &str,
    max_width: usize,
    first_visual_len: usize,
    rest_visual_len: usize,
    style_line: F,
) -> Vec<String> {
    let words = wrap_tokens(text);
    if words.is_empty() {
        return vec![];
    }
    let (mut lines, mut cur, mut cur_len) = (Vec::new(), String::new(), 0usize);
    let mut limit = max_width.saturating_sub(first_visual_len).max(30);
    for w in words {
        let w_len = str_display_width(&w);
        if cur.is_empty() {
            cur = w;
            cur_len = w_len;
        } else if cur_len + 1 + w_len <= limit {
            cur.push(' ');
            cur.push_str(&w);
            cur_len += 1 + w_len;
        } else {
            let pfx = if lines.is_empty() { first_prefix } else { rest_prefix };
            lines.push(format!("{pfx}{}", style_line(&cur)));
            cur = w;
            cur_len = w_len;
            limit = max_width.saturating_sub(rest_visual_len).max(30);
        }
    }
    if !cur.is_empty() {
        let pfx = if lines.is_empty() { first_prefix } else { rest_prefix };
        lines.push(format!("{pfx}{}", style_line(&cur)));
    }
    lines
}

pub struct DiagnosticView<'a> {
    pub file: &'a str,
    pub line: usize,
    /// Optional `(start_col_0based_utf16, end_col_0based_utf16)` on `line`.
    pub cols: Option<(usize, usize)>,
    pub code: &'a str,
    pub rule: &'a str,
    pub msg: &'a str,
    pub source: Option<&'a str>,
}

fn utf16_col_to_visual(raw: &str, target_utf16: usize) -> usize {
    let (mut u16, mut vis) = (0usize, 0usize);
    for ch in raw.chars() {
        if u16 >= target_utf16 {
            break;
        }
        u16 += ch.len_utf16();
        vis += char_display_width(ch);
    }
    vis
}

/// Returns `(effective_line, loc_col0_u16, vis_col0, vis_len, expanded_source_line)`.
fn resolve_snippet(src: Option<&str>, line: usize, cols: Option<(usize, usize)>) -> (usize, usize, usize, usize, Option<String>) {
    let line = line.max(1);
    let Some(src) = src else {
        let (c0, c1) = cols.unwrap_or((0, 1));
        return (line, c0, c0, c1.saturating_sub(c0).max(1), None);
    };
    let lines: Vec<&str> = src.lines().collect();
    if let Some(&raw) = lines.get(line - 1) {
        let expanded = raw.replace('\t', "  ");
        let raw_u16_len: usize = raw.chars().map(char::len_utf16).sum();
        let (raw_col0, raw_col1) = if let Some((c0, c1)) = cols {
            let c0 = c0.min(raw_u16_len);
            (c0, c1.min(raw_u16_len).max(c0))
        } else {
            let leading: usize = raw.chars().take_while(|c| c.is_whitespace()).map(char::len_utf16).sum();
            let trimmed = raw.find(" //").map_or_else(|| raw.trim_end(), |i| raw[..i].trim_end());
            let end: usize = trimmed.chars().map(char::len_utf16).sum();
            (leading, end.max(leading + 1))
        };
        let (vis0, vis1) = (utf16_col_to_visual(raw, raw_col0), utf16_col_to_visual(raw, raw_col1));
        return (line, raw_col0, vis0, vis1.saturating_sub(vis0).max(1), Some(expanded));
    }
    // EOF parse error after trailing newline (`line > lines.len()`): anchor caret to the end of the last non-empty line.
    if let Some((idx, &raw)) = lines.iter().enumerate().rev().find(|(_, l)| !l.trim().is_empty()) {
        let end_u16: usize = raw.trim_end().chars().map(char::len_utf16).sum();
        let vis_end = utf16_col_to_visual(raw, end_u16);
        return (idx + 1, end_u16, vis_end, 1, Some(raw.replace('\t', "  ")));
    }
    (line, 0, 0, 1, None)
}

pub fn format_diagnostic(d: &DiagnosticView<'_>, compact: bool, color: bool) -> String {
    format_diagnostic_with_width(d, compact, color, 96)
}

pub fn format_diagnostic_with_width(d: &DiagnosticView<'_>, compact: bool, color: bool, max_width: usize) -> String {
    let (eff_line, loc_col0, vis_col0, vis_len, src_line) = resolve_snippet(d.source, d.line, d.cols);
    let has_col = d.cols.is_some() || src_line.is_some();
    let loc = if has_col { format!("{}:{}:{}", d.file, eff_line, loc_col0 + 1) } else { format!("{}:{}", d.file, eff_line) };

    if compact {
        return format!("{loc}: error {} [{}] {}", d.code, d.rule, d.msg);
    }

    let mut parts = split_message(d.msg);
    if let Some(stripped) = parts.head.strip_suffix(&format!(" ({})", d.rule)) {
        parts.head = stripped.to_string();
    }

    let gutter_w = eff_line.to_string().len().max(2);
    let pad = " ".repeat(gutter_w);
    let head_plain = format!("error[{}] ({}): ", d.code, d.rule);
    let head_pfx = if color {
        format!("{ANSI_RED}error[{}]{ANSI_RESET} {ANSI_DIM}({}){ANSI_RESET}{ANSI_BOLD}: {ANSI_RESET}", d.code, d.rule)
    } else {
        head_plain.clone()
    };

    let head_state = Cell::new(false);
    let mut out = wrap_with_prefix(&parts.head, &head_pfx, "    ", max_width, head_plain.chars().count(), 4, |s| {
        if color { style_prose_ansi(s, ANSI_BOLD, &head_state) } else { s.to_string() }
    });

    out.push(if color { format!("{pad}{ANSI_GUTTER}-->{ANSI_RESET} {loc}") } else { format!("{pad}--> {loc}") });

    if let Some(src_text) = src_line.filter(|s| !s.trim().is_empty()) {
        let lnum = format!("{eff_line:>gutter_w$}");
        let carets = format!("{}{}", " ".repeat(vis_col0), "^".repeat(vis_len));
        if color {
            out.push(format!("{ANSI_GUTTER}{lnum} |{ANSI_RESET} {src_text}"));
            out.push(format!("{pad} {ANSI_GUTTER}|{ANSI_RESET} {ANSI_RED}{carets}{ANSI_RESET}"));
        } else {
            out.push(format!("{lnum} | {src_text}"));
            out.push(format!("{pad} | {carets}"));
        }
    }

    if parts.help.len() == 1 {
        let (p_plain, c_plain) = (format!("{pad} = help: "), format!("{pad}         "));
        let p_col = if color { format!("{pad} {ANSI_GUTTER}={ANSI_RESET} {ANSI_BLUE}help:{ANSI_RESET} ") } else { p_plain.clone() };
        let state = Cell::new(false);
        out.extend(wrap_with_prefix(&parts.help[0], &p_col, &c_plain, max_width, p_plain.chars().count(), c_plain.chars().count(), |s| {
            if color { style_prose_ansi(s, "", &state) } else { s.to_string() }
        }));
    } else if parts.help.len() > 1 {
        for (i, fx) in parts.help.iter().enumerate() {
            let p_plain = if i == 0 { format!("{pad} = help: • ") } else { format!("{pad}         • ") };
            let c_plain = format!("{pad}           ");
            let p_col = match (color, i == 0) {
                (true, true) => format!("{pad} {ANSI_GUTTER}={ANSI_RESET} {ANSI_BLUE}help:{ANSI_RESET} {ANSI_BLUE}•{ANSI_RESET} "),
                (true, false) => format!("{pad}         {ANSI_BLUE}•{ANSI_RESET} "),
                (false, _) => p_plain.clone(),
            };
            let state = Cell::new(false);
            out.extend(wrap_with_prefix(fx, &p_col, &c_plain, max_width, p_plain.chars().count(), c_plain.chars().count(), |s| {
                if color { style_prose_ansi(s, "", &state) } else { s.to_string() }
            }));
        }
    }

    for note in &parts.notes {
        let (p_plain, c_plain) = (format!("{pad} = note: "), format!("{pad}         "));
        let p_col = if color { format!("{pad} {ANSI_GUTTER}={ANSI_RESET} {ANSI_DIM}note:{ANSI_RESET} ") } else { p_plain.clone() };
        let state = Cell::new(false);
        out.extend(wrap_with_prefix(note, &p_col, &c_plain, max_width, p_plain.chars().count(), c_plain.chars().count(), |s| {
            if color { style_prose_ansi(s, "", &state) } else { s.to_string() }
        }));
    }

    out.join("\n")
}

pub fn format_docs_footer(codes: &[String], color: bool) -> String {
    if std::env::var("RX_DOCS").is_ok_and(|v| v == "0") {
        return String::new();
    }
    let base = std::env::var("RX_GUIDE_BASE").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| GUIDE_BASE.to_string());
    let mut seen = Vec::new();
    for c in codes {
        if !seen.contains(c) {
            seen.push(c.clone());
        }
    }
    seen.into_iter()
        .map(|c| {
            let url = format!("{base}#{}", c.to_lowercase());
            if color { format!("{ANSI_DIM}docs[{c}]: {url}{ANSI_RESET}") } else { format!("docs[{c}]: {url}") }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_held_write_syntactically_into_head_and_four_help_bullets() {
        let d = split_message(
            "status := … comes before an `await` in a body that is an action because it writes and awaits (AC-1b), so it is held until the body ends (AC-1): for state that shows while the work runs, `guess status := …` (a source with a question, ST-5c); for a reset that lands now, `onFlush(() => { status := … })` (EF-8); to hold it on purpose, declare the action (`action name() { … }`); or write it after the `await`",
        );
        assert_eq!(
            d.head,
            "status := … comes before an `await` in a body that is an action because it writes and awaits (AC-1b), so it is held until the body ends (AC-1)"
        );
        assert_eq!(d.help.len(), 4);
        assert!(d.notes.is_empty());
    }

    #[test]
    fn preserves_multi_colon_head_before_semicolon_disjunction_and_ignores_declarative_semicolons() {
        let multi = split_message("x is read: the read is stale: use `a`; or use `b`");
        assert_eq!(multi.head, "x is read: the read is stale");
        assert_eq!(multi.help, vec!["use `a`", "or use `b`"]);

        let declarative = split_message("markup at a module's top level: a module is declarations; the app's view is a component");
        assert_eq!(declarative.head, "markup at a module's top level: a module is declarations; the app's view is a component");
        assert!(declarative.help.is_empty());
    }

    #[test]
    fn keeps_paired_parenthetical_em_dashes_intact_in_client_not_awaited() {
        let msg = "measure(…) is `client` JS: in reactive scope it is awaited — a client question, asked once the browser has the page (SV-7b) — never called for its value now";
        let d = split_message(msg);
        assert_eq!(d.head, msg);
        assert!(d.help.is_empty());
        assert!(d.notes.is_empty());
    }

    #[test]
    fn classifies_causal_and_rationale_tails_and_trailing_sentences_as_notes() {
        let io = split_message("save(…) is io and is called in reactive scope (R-io-3) — its server body calls dbWrite");
        assert_eq!(io.head, "save(…) is io and is called in reactive scope (R-io-3)");
        assert!(io.help.is_empty());
        assert_eq!(io.notes, vec!["its server body calls dbWrite"]);

        let css5 = split_message(
            "CSS-5: a class map is a declaration, `styles = <style>…</style>` — TSRX's `<style ref={styles}>` assigns into a variable, and a name here is bound once, by its declaration",
        );
        assert_eq!(css5.head, "a class map is a declaration, `styles = <style>…</style>`");
        assert!(css5.help.is_empty());
        assert_eq!(css5.notes.len(), 1);

        let spa = split_message(
            "a reader of user can have no value yet: in a client-only build the first load would show nothing until it answers — put it under `loading fallback { … } { … }`, or give the await a first paint (`await f() -> v`). Later answers are transitions and need no region",
        );
        assert_eq!(spa.help.len(), 2);
        assert!(!spa.help[1].contains("Later answers"));
        assert_eq!(spa.notes, vec!["Later answers are transitions and need no region"]);
    }

    #[test]
    fn strips_raw_near_slice_even_when_excerpt_contains_quotes_and_em_dash() {
        let d = split_message(
            "expected expression near ':= n + 1}>{n}</butto' — `:=` is a statement, so an event handler's body is `{ n := n + 1 }`, in braces: `() => expr` is an expression",
        );
        assert_eq!(d.head, "expected expression");
        assert_eq!(d.help.len(), 1);

        let tricky = split_message("expected ';' near ''a' — b' — a source is written with `:=`; .rx has no `=`, `+=` or `++` on state");
        assert_eq!(tricky.head, "expected ';'");
        assert_eq!(tricky.help, vec!["a source is written with `:=`; .rx has no `=`, `+=` or `++` on state"]);
    }

    #[test]
    fn normalizes_nested_template_backticks_and_handles_unspaced_less_than() {
        let nested = split_message(
            "`await `…`` inside an expression of imperative scope: a run suspends only at a statement — bind it first, `let v = await `…`` (a `catch (e) => alt` may follow it)",
        );
        assert_eq!(nested.head.chars().filter(|&c| c == '`').count() % 2, 0);
        assert_eq!(nested.help[0].chars().filter(|&c| c == '`').count() % 2, 0);

        let lt = split_message("index x<n is out of range — check bounds first");
        assert_eq!(lt.head, "index x<n is out of range");
        assert_eq!(lt.help, vec!["check bounds first"]);
    }

    #[test]
    fn keeps_semicolons_inside_backticks_intact() {
        let d = split_message(
            "count is read after this change wrote it: the read sees the committed value, not the write (W-4) — keep the value you wrote in a local (`let v = …; count := v`) and read that, or write functionally (`count := v => …`, W-6)",
        );
        assert_eq!(d.help.len(), 2);
        assert!(d.help[0].contains("`let v = …; count := v`"));
    }

    #[test]
    fn formats_block_with_wide_emoji_cjk_tabs_and_eof_trailing_newline() {
        let src = "\t<button onClick={() => { double := 4 }}>🔥 {double}</button>\n";
        let view = DiagnosticView { file: "counter.rx", line: 1, cols: Some((1, 59)), code: "WRITE_TO_NON_SOURCE", rule: "W-1", msg: "double is an equation: only sources are written", source: Some(src) };
        let human = format_diagnostic(&view, false, false);
        assert!(human.contains("--> counter.rx:1:2"));
        assert!(human.contains(&format!("   |   {}", "^".repeat(58))), "got:\n{human}");

        let eof_src = "export fn broken( {\n";
        let eof_view = DiagnosticView { file: "catalog.rx", line: 2, cols: Some((0, 0)), code: "PARSE", rule: "07", msg: "expected identifier", source: Some(eof_src) };
        let eof_human = format_diagnostic(&eof_view, false, false);
        assert!(eof_human.contains("--> catalog.rx:1:20"), "got:\n{eof_human}");
        assert!(eof_human.contains(" 1 | export fn broken( {"));
        assert!(eof_human.contains("   |                    ^"));
    }
}
