//! Integration tests for `format`: `rx before` examples in `guide/diagnostics.md` (SSR + SPA), multi-module diagnostics,
//! all 5 `<pre class="err">` blocks in `site/index.html`, `serve::analyze`, and `rxc check` piped vs `--pretty` CLI behavior.

use reactivescript::check::check_modules;
use reactivescript::format::{self, DiagnosticView};
use reactivescript::modules::load_modules;
use reactivescript::serve;
use reactivescript::spans;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn balanced_backticks(s: &str) -> bool {
    s.chars().filter(|&c| c == '`').count() % 2 == 0
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn prose_ansi_to_html(body: &str, base: &str) -> String {
    let close_code = format!("`\x1b[0m{base}");
    let close_dim = format!("\x1b[0m{base}");
    let (mut out, mut in_code, mut rest) = (String::new(), false, body);
    while !rest.is_empty() {
        if !in_code {
            if let Some(r) = rest.strip_prefix("\x1b[0m\x1b[36m`") {
                out.push_str(r#"<span class="ec">"#);
                in_code = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("\x1b[0m\x1b[36m") {
                out.push_str(r#"<span class="ec">"#);
                in_code = true;
                rest = r;
                continue;
            }
            if let Some(after) = rest.strip_prefix("\x1b[0m\x1b[2m") {
                if let Some(i) = after.find(&close_dim) {
                    out.push_str(&format!(r#"<span class="ed">{}</span>"#, esc(&after[..i])));
                    rest = &after[i + close_dim.len()..];
                    continue;
                }
            }
        } else {
            if let Some(r) = rest.strip_prefix(&close_code) {
                out.push_str("</span>");
                in_code = false;
                rest = r;
                continue;
            }
            if rest == "\x1b[0m" {
                out.push_str("</span>");
                break;
            }
        }
        let ch = rest.chars().next().unwrap();
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Convert `format_diagnostic_with_width(..., color = true)` ANSI output into `site/index.html`'s `<span class="…">` markup.
fn ansi_to_site_html(ansi: &str) -> String {
    ansi.lines()
        .map(|line| {
            if let Some(r) = line.strip_prefix("\x1b[1;31m") {
                let (eh, r) = r.split_once("\x1b[0m \x1b[2m").unwrap();
                let (ed, r) = r.split_once("\x1b[0m\x1b[1m: \x1b[0m\x1b[1m").unwrap();
                let body = r.strip_suffix("\x1b[0m").unwrap();
                return format!(r#"<span class="eh">{}</span> <span class="ed">{}</span>: <span class="em">{}</span>"#, esc(eh), esc(ed), prose_ansi_to_html(body, "\x1b[1m"));
            }
            if let Some(r) = line.strip_prefix("    \x1b[1m") {
                let body = r.strip_suffix("\x1b[0m").unwrap();
                return format!(r#"    <span class="em">{}</span>"#, prose_ansi_to_html(body, "\x1b[1m"));
            }
            if let Some((pad, loc)) = line.split_once("\x1b[2;36m-->\x1b[0m ") {
                return format!(r#"{pad}<span class="eg">--&gt; {}</span>"#, esc(loc));
            }
            if let Some(r) = line.strip_prefix("\x1b[2;36m") {
                if let Some((lnum, src)) = r.split_once(" |\x1b[0m ") {
                    return format!(r#"<span class="eg">{lnum} |</span> <span class="es">{}</span>"#, esc(src));
                }
            }
            if let Some((pad, r)) = line.split_once(" \x1b[2;36m|\x1b[0m \x1b[1;31m") {
                let carets = r.strip_suffix("\x1b[0m").unwrap();
                return format!(r#"{pad} <span class="eg">|</span> <span class="eu">{carets}</span>"#);
            }
            for (ansi_pfx, html_pfx) in [
                (" \x1b[2;36m=\x1b[0m \x1b[36mhelp:\x1b[0m \x1b[36m•\x1b[0m ", r#" <span class="eg">=</span> <span class="ef">help:</span> <span class="ef">•</span> "#),
                (" \x1b[2;36m=\x1b[0m \x1b[36mhelp:\x1b[0m ", r#" <span class="eg">=</span> <span class="ef">help:</span> "#),
                ("         \x1b[36m•\x1b[0m ", r#"         <span class="ef">•</span> "#),
                (" \x1b[2;36m=\x1b[0m \x1b[2mnote:\x1b[0m ", r#" <span class="eg">=</span> <span class="ed">note:</span> "#),
            ] {
                if let Some((pad, body)) = line.split_once(ansi_pfx) {
                    return format!("{pad}{html_pfx}{}", prose_ansi_to_html(body, ""));
                }
            }
            let indent = line.len() - line.trim_start().len();
            format!("{}{}", &line[..indent], prose_ansi_to_html(&line[indent..], ""))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_clean_diag(d: &serde_json::Value) {
    let code = d["code"].as_str().unwrap();
    let head = d["head"].as_str().unwrap();
    assert!(!head.is_empty(), "{code}: empty head");
    assert!(!head.contains(" near '"), "{code}: raw near '...' leaked into head: {head}");
    assert!(balanced_backticks(head), "{code}: unbalanced backticks in head: {head}");
    for h in d["help"].as_array().unwrap() {
        assert!(balanced_backticks(h.as_str().unwrap()), "{code}: unbalanced backticks in help: {h}");
    }
    for n in d["notes"].as_array().unwrap() {
        assert!(balanced_backticks(n.as_str().unwrap()), "{code}: unbalanced backticks in note: {n}");
    }
    let rendered = d["rendered"].as_str().expect("rendered must be present");
    assert!(rendered.starts_with(&format!("error[{code}] (")), "unexpected header: {rendered}");
    assert!(rendered.contains("--> "), "missing location arrow: {rendered}");
    assert!(rendered.contains('^'), "missing caret underline: {rendered}");
}

#[test]
fn every_guide_and_multimodule_diagnostic_splits_and_renders_cleanly() {
    let page = std::fs::read_to_string(root().join("guide/diagnostics.md")).unwrap();
    let mut fence: Option<(String, String)> = None;
    let mut count = 0usize;
    for line in page.lines() {
        if let Some((mark, body)) = &mut fence {
            if line == "```" {
                if mark == "before" {
                    let src = body.clone();
                    let entry = "/page/main.rx";
                    let read = |p: &str| (p == entry).then(|| src.clone());
                    let diags: Vec<serde_json::Value> = match load_modules(entry, &read) {
                        Err(e) => vec![serve::parse_error_in_rendered(entry, &e, Some(&src))],
                        Ok(set) => {
                            let mut ds = check_modules(&set, false).diags;
                            if ds.is_empty() {
                                ds = check_modules(&set, true).diags;
                            }
                            ds.iter().map(|d| serve::diagnostic_json_rendered(&set, d)).collect()
                        }
                    };
                    for d in &diags {
                        count += 1;
                        assert_clean_diag(d);
                    }
                }
                fence = None;
            } else {
                body.push_str(line);
                body.push('\n');
            }
            continue;
        }
        if let Some(mark) = line.strip_prefix("```rx ") {
            fence = Some((mark.trim().to_string(), String::new()));
        }
    }
    assert!(count >= 45, "expected >= 45 guide diagnostics, got {count}");

    let files = HashMap::from([
        ("/app/api.d.rx".to_string(), "export client fn measure(): number\nexport server fn dbWrite(): void\n".to_string()),
        (
            "/app/main.rx".to_string(),
            "import { measure, dbWrite } from \"./api\"\nserver fn save() {\n  dbWrite()\n}\nexport default component App() {\n  w = measure()\n  x = save()\n  action go() {\n    let s = `${await save()}`\n  }\n  <p>{w} {x}</p>\n}\n".to_string(),
        ),
    ]);
    let set = load_modules("/app/main.rx", &|p| files.get(p).cloned()).unwrap();
    let multi_diags: Vec<_> = check_modules(&set, false).diags.iter().map(|d| serve::diagnostic_json_rendered(&set, d)).collect();
    assert!(multi_diags.iter().any(|d| d["code"] == "CLIENT_NOT_AWAITED" && d["help"].as_array().unwrap().is_empty()));
    assert!(multi_diags.iter().any(|d| d["code"] == "IO_IN_REACTIVE" && !d["notes"].as_array().unwrap().is_empty()));
    assert!(multi_diags.iter().any(|d| d["code"] == "AWAIT_IN_EXPRESSION"));
    for d in &multi_diags {
        assert_clean_diag(d);
    }

    let analyzed = serve::analyze(
        &json!({ "file": "/app/c.rx", "overlays": { "/app/c.rx": "export default component C() {\n  x = 1\n  <button onClick={() => { x := 2 }}>{x}</button>\n}\n" } }),
        &|_| None,
    );
    let ad = &analyzed["diagnostics"][0];
    assert_eq!(ad["code"], "WRITE_TO_NON_SOURCE");
    assert!(ad["rendered"].as_str().unwrap().contains("error[WRITE_TO_NON_SOURCE] (W-1):"));
}

#[test]
fn site_index_html_error_blocks_match_formatted_ansi_diagnostics() {
    let html = std::fs::read_to_string(root().join("site/index.html")).unwrap();
    let re = Regex::new(r#"(?s)<pre class="err"><code>(.*?)</code></pre>"#).unwrap();
    let blocks: Vec<String> = re.captures_iter(&html).map(|c| c[1].to_string()).collect();
    assert_eq!(blocks.len(), 5, "site/index.html should contain 5 <pre class=\"err\"> blocks");

    struct Case<'a> {
        file: &'a str,
        check_src: &'a str,
        excerpt: Option<(&'a str, usize)>,
    }

    let cases = [
        Case {
            file: "counter.rx",
            check_src: "export default component Counter() {\n  source count := 0\n  double = count * 2\n\n  <button onClick={() => { double := 4 }}>{count} × 2 = {double}</button>\n}",
            excerpt: None,
        },
        Case {
            file: "profile.rx",
            check_src: "import pure { fetchUser } from \"./api\"\n\nexport default component Profile() {\n  source id := 1\n  user = await fetchUser(id)              // re-asked whenever `id` changes\n\n  <section>\n    <button onClick={() => { id := id + 1 }}>Next</button>\n    {loading fallback { <p>Loading…</p> } {\n      <h1>{user.name}</h1>\n    }}\n  </section>\n}",
            excerpt: None,
        },
        Case {
            file: "like.rx",
            check_src: "import pure { fetchLikes } from \"./api\"\nimport { like } from \"./api\"\n\nexport default component Like(props: { id: number }) {\n  likes = await fetchLikes(props.id)\n\n  action send() {\n    guess likes := likes + 1        // show it now; the server's answer replaces it\n    await like(props.id)\n    refresh likes\n  }\n\n  <button onClick={() => { send() }}>♥ {likes}</button>\n}",
            excerpt: None,
        },
        Case {
            file: "save.rx",
            check_src: "import { saveName } from \"./api\"\nexport default component Save() {\n  source status := \"Idle\"\n  <button onClick={() => {\n    status := \"Saving…\"\n    let r = await saveName(\"Ann\") catch (e) => e.message\n    status := r\n  }}>{status}</button>\n}",
            excerpt: Some(("\n\n\n<button onClick={() => {\n  status := \"Saving…\"\n  let r = await saveName(\"Ann\") catch (e) => e.message\n  status := r\n}}>{status}</button>", 5)),
        },
        Case {
            file: "tags.rx",
            check_src: "import { addTag } from \"./api\"\nexport default component Tags() {\n  source tags := \"rx\"\n  action add(t: string) {\n    let before = tags\n    let saved = await addTag(t) catch (e) => t\n    tags := before + \" \" + saved\n  }\n  <p>{tags}</p>\n}",
            excerpt: Some(("\naction add(t: string) {\n  let before = tags\n  let saved = await addTag(t) catch (e) => t\n  tags := before + \" \" + saved\n}", 5)),
        },
    ];

    for (idx, c) in cases.iter().enumerate() {
        let entry = "/site/main.rx";
        let set = load_modules(entry, &|p| (p == entry).then(|| c.check_src.to_string())).unwrap();
        let prog = &set.programs[entry];
        let diags = check_modules(&set, false).diags;
        let expected = diags
            .iter()
            .map(|d| {
                let (line, cols, src) = if let Some((ex, shown_line)) = c.excerpt {
                    (shown_line, None, ex)
                } else {
                    let line = d.line.max(1) as usize;
                    let r = spans::line_range(c.check_src, prog, line).unwrap();
                    let ((_, c0), (_, c1)) = (spans::line_col(c.check_src, r.start), spans::line_col(c.check_src, r.end));
                    (line, Some((c0, c1)), c.check_src)
                };
                let view = DiagnosticView { file: c.file, line, cols, code: &d.code, rule: &d.rule, msg: &d.msg, source: Some(src) };
                ansi_to_site_html(&format::format_diagnostic_with_width(&view, false, true, 94))
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(blocks[idx], expected, "site/index.html block #{} ({}) drifted", idx + 1, c.file);
    }
}

#[test]
fn rxc_check_piped_is_compact_and_pretty_flag_renders_blocks() {
    let dir = std::env::temp_dir().join(format!("rxc-fmt-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rx_file = dir.join("profile.rx");
    std::fs::write(
        &rx_file,
        "import pure { fetchUser } from \"./api\"\nexport default component Profile() {\n  user = await fetchUser(1)\n  {loading fallback { <p>…</p> } { <h1>{user.name}</h1> }}\n}\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_rxc");

    let piped = Command::new(bin).arg("check").arg(&rx_file).env("FORCE_COLOR", "1").output().unwrap();
    assert!(!piped.status.success());
    let piped_err = String::from_utf8_lossy(&piped.stderr);
    assert!(!piped_err.contains("\x1b["), "piped compact output must never include ANSI codes");
    assert!(piped_err.contains(":4:3: error UNHANDLED_FAILURE [R-f-5] "), "got: {piped_err}");
    assert!(!piped_err.contains("= help:"));
    assert!(!piped_err.contains("docs["));

    let pretty = Command::new(bin).args(["check", "--pretty"]).arg(&rx_file).env("FORCE_COLOR", "false").output().unwrap();
    assert!(!pretty.status.success());
    let pretty_err = String::from_utf8_lossy(&pretty.stderr);
    assert!(!pretty_err.contains("\x1b["), "FORCE_COLOR=false must not emit ANSI codes");
    assert!(pretty_err.contains("error[UNHANDLED_FAILURE] (R-f-5):"), "got: {pretty_err}");
    assert!(pretty_err.contains("= help: • wrap it in `errored fallback(e) { … } { … }`"));
    assert!(pretty_err.contains("docs[UNHANDLED_FAILURE]:"));

    let json_out = Command::new(bin).args(["check", "--json"]).arg(&rx_file).output().unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json_out.stdout).unwrap();
    let d0 = &parsed["diagnostics"][0];
    assert!(d0["head"].is_string());
    assert!(d0["help"].is_array());
    assert!(d0["notes"].is_array());
    assert!(d0["rendered"].is_null());

    let _ = std::fs::remove_dir_all(&dir);
}
