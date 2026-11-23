use flowmaid::{SvgOptions, style::Theme};
use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};
use pulldown_latex::{
    Parser as LatexParser, RenderConfig, Storage as LatexStorage, config::DisplayMode, push_mathml,
};
use std::borrow::Cow;

pub fn render(markdown: &str) -> String {
    if markdown.is_empty() {
        return String::new();
    }
    let normalized = normalize_math_delimiters(markdown);
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
        | Options::ENABLE_MATH;

    let parser = Parser::new_ext(&normalized, opts);
    let mut transformed = Vec::new();
    let mut code_block: Option<(String, String)> = None;

    for event in parser {
        if let Some((ref lang, ref mut buf)) = code_block {
            match event {
                Event::Text(t) | Event::Code(t) => buf.push_str(&t),
                Event::SoftBreak | Event::HardBreak => buf.push('\n'),
                Event::End(TagEnd::CodeBlock) => {
                    let html_block = render_code_block(lang, buf);
                    code_block = None;
                    transformed.push(Event::Html(CowStr::Boxed(html_block.into_boxed_str())));
                }
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                };
                code_block = Some((lang, String::new()));
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                transformed.push(Event::Text(raw));
            }
            Event::InlineMath(tex) => {
                let rendered = render_latex(&tex, false);
                transformed.push(Event::InlineHtml(CowStr::Boxed(rendered.into_boxed_str())));
            }
            Event::DisplayMath(tex) => {
                let rendered = format!(
                    "<span class=\"math-display\">{}</span>",
                    render_latex(&tex, true)
                );
                transformed.push(Event::InlineHtml(CowStr::Boxed(rendered.into_boxed_str())));
            }
            other => transformed.push(other),
        }
    }

    if let Some((lang, buf)) = code_block {
        let html_block = render_code_block(&lang, &buf);
        transformed.push(Event::Html(CowStr::Boxed(html_block.into_boxed_str())));
    }

    let mut out = String::with_capacity(markdown.len() + 128);
    html::push_html(&mut out, transformed.into_iter());
    out
}

fn render_code_block(lang: &str, code: &str) -> String {
    let trimmed_lang = lang.trim();
    if trimmed_lang.eq_ignore_ascii_case("mermaid") {
        let mut opts = SvgOptions::default();
        opts.theme = Some(Theme::Dark);
        opts.transparent = true;
        opts.responsive = true;
        if let Ok(svg) = flowmaid::render_svg_advanced(code, &opts) {
            let mut out = String::with_capacity(svg.len() + code.len() + 192);
            out.push_str("<div class=\"code-block mermaid-block\"><div class=\"code-head\"><span class=\"code-lang\">mermaid</span><button type=\"button\" class=\"copy-code\">Copy</button></div><pre class=\"mermaid-src\" hidden><code>");
            push_escaped(&mut out, code);
            out.push_str("</code></pre><div class=\"mermaid-svg\">");
            out.push_str(&svg);
            out.push_str("</div></div>");
            return out;
        }
    }

    let mut out = String::with_capacity(code.len() + 160);
    out.push_str("<div class=\"code-block\"><div class=\"code-head\"><span class=\"code-lang\">");
    if !trimmed_lang.is_empty() {
        push_escaped(&mut out, trimmed_lang);
    }
    out.push_str(
        "</span><button type=\"button\" class=\"copy-code\">Copy</button></div><pre><code>",
    );
    push_escaped(&mut out, code);
    out.push_str("</code></pre></div>");
    out
}

fn render_latex(tex: &str, display: bool) -> String {
    let storage = LatexStorage::new();
    let parser = LatexParser::new(tex, &storage);
    let cfg = RenderConfig {
        display_mode: if display {
            DisplayMode::Block
        } else {
            DisplayMode::Inline
        },
        ..Default::default()
    };
    let mut mathml = String::new();
    if push_mathml(&mut mathml, parser, cfg).is_ok() && !mathml.is_empty() {
        mathml
    } else {
        let mut fallback = String::from("<code>");
        push_escaped(&mut fallback, tex);
        fallback.push_str("</code>");
        fallback
    }
}

pub fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

fn normalize_math_delimiters(input: &str) -> Cow<'_, str> {
    if !input.contains("\\[") && !input.contains("\\(") {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len() + 16);
    let mut i = 0;
    let mut in_fence = false;
    let mut in_inline_code = false;

    while i < bytes.len() {
        if !in_inline_code
            && (i == 0 || bytes[i - 1] == b'\n')
            && i + 3 <= bytes.len()
            && (&bytes[i..i + 3] == b"```" || &bytes[i..i + 3] == b"~~~")
        {
            in_fence = !in_fence;
            out.push_str(&input[i..i + 3]);
            i += 3;
            continue;
        }
        if !in_fence && bytes[i] == b'`' {
            in_inline_code = !in_inline_code;
            out.push('`');
            i += 1;
            continue;
        }
        if !in_fence && !in_inline_code && bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' | b']' => {
                    out.push_str("$$");
                    i += 2;
                    continue;
                }
                b'(' | b')' => {
                    out.push('$');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        let ch = input[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown_code_copy_latex_mermaid_and_escapes_xss() {
        let md = "Hello **world** $\\frac{1}{2}$ and \\[x^2\\]\n\n```rust\nfn main() {}\n```\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n<script>alert(1)</script>";
        let html = render(md);
        assert!(html.contains("<strong>world</strong>"));
        assert!(html.contains("<math"));
        assert!(html.contains("class=\"copy-code\""));
        assert!(html.contains("<svg"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
