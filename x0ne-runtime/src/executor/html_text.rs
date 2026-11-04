//! HTML to plain markdown for `FetchUrl`.
//! Hand-rolled: Cargo.lock had no HTML parser on 2026-10-06, and this
//! output feeds a model, not a browser.

/// Bytes of text handed back to the model per FetchUrl call.
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Convert `html` to markdown, resolving links against `base`.
pub(crate) fn html_to_markdown(html: &str, base: Option<&str>) -> String {
    convert(html, base, false)
}

/// Convert `html` to plain text: no markup, link targets dropped.
pub(crate) fn html_to_text(html: &str, base: Option<&str>) -> String {
    convert(html, base, true)
}

fn convert(html: &str, base: Option<&str>, plain: bool) -> String {
    let mut c = Converter {
        out: String::with_capacity(html.len() / 4),
        base: base.and_then(|b| url::Url::parse(b).ok()),
        plain,
        pre: 0,
        fence_at: None,
        links: Vec::new(),
    };
    c.run(html);
    tidy(&c.out)
}

/// One bounded slice of a longer text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub body: String,
    pub total: usize,
    pub offset: usize,
    /// Where the next read starts; `None` when this slice reaches the end.
    pub next_offset: Option<usize>,
}

/// Cut `text` to at most `max` bytes from `offset`, on char and line
/// boundaries, and append a note naming the next offset when cut.
pub(crate) fn window(text: &str, offset: usize, max: usize) -> Window {
    let total = text.len();
    let mut start = offset.min(total);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start.saturating_add(max).min(total);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < total {
        // Cut after the last newline when one exists in the window.
        if let Some(nl) = text[start..end].rfind('\n') {
            end = start + nl + 1;
        }
    }
    let slice = text[start..end].trim_start_matches('\n');
    if end >= total {
        return Window {
            body: slice.to_string(),
            total,
            offset: start,
            next_offset: None,
        };
    }
    Window {
        body: format!(
            "{}\n\n[truncated: bytes {}..{} of {}; call FetchUrl again with offset: {} to read on]",
            slice.trim_end(),
            start,
            end,
            total,
            end
        ),
        total,
        offset: start,
        next_offset: Some(end),
    }
}

/// Elements whose whole subtree is dropped.
const SKIP: &[&str] = &[
    "head", "nav", "footer", "svg", "template", "iframe", "object", "canvas", "noscript",
];
/// Elements whose content is raw text: skip to the matching close tag.
const RAW_TEXT: &[&str] = &["script", "style", "textarea"];
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "aside",
    "ul",
    "ol",
    "dl",
    "table",
    "blockquote",
    "figure",
    "figcaption",
    "hr",
    "details",
    "summary",
    "form",
    "fieldset",
    "address",
    "center",
];

struct Converter {
    out: String,
    base: Option<url::Url>,
    /// Plain text: no `#`, fences, backticks or link targets.
    plain: bool,
    pre: usize,
    fence_at: Option<usize>,
    links: Vec<(usize, Option<String>)>,
}

struct Tag<'a> {
    name: String,
    closing: bool,
    self_closing: bool,
    attrs: &'a str,
}

impl Converter {
    fn run(&mut self, html: &str) {
        let mut i = 0;
        while i < html.len() {
            let rest = &html[i..];
            let Some(lt) = rest.find('<') else {
                self.text(rest);
                break;
            };
            if lt > 0 {
                self.text(&rest[..lt]);
                i += lt;
                continue;
            }
            match parse_tag(rest) {
                Markup::Comment(len) => i += len,
                Markup::Text => {
                    self.text("<");
                    i += 1;
                }
                Markup::Tag(tag, len) => {
                    i += len;
                    if !tag.closing && RAW_TEXT.contains(&tag.name.as_str()) {
                        i += skip_raw(&html[i..], &tag.name);
                    } else if !tag.closing && !tag.self_closing && SKIP.contains(&tag.name.as_str())
                    {
                        i += skip_subtree(&html[i..], &tag.name);
                    } else {
                        self.tag(&tag);
                    }
                }
            }
        }
    }

    fn text(&mut self, raw: &str) {
        let decoded = decode_entities(raw);
        if self.pre > 0 {
            self.out.push_str(&decoded);
            return;
        }
        for ch in decoded.chars() {
            if ch.is_whitespace() {
                if !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
                    self.out.push(' ');
                }
            } else {
                self.out.push(ch);
            }
        }
    }

    fn block(&mut self) {
        trim_trailing_spaces(&mut self.out);
        if self.out.is_empty() {
            return;
        }
        while !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn line(&mut self) {
        trim_trailing_spaces(&mut self.out);
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn tag(&mut self, t: &Tag) {
        let name = t.name.as_str();
        if self.pre > 0 && name != "pre" && name != "code" {
            if name == "br" {
                self.out.push('\n');
            }
            return;
        }
        match (name, t.closing) {
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => {
                self.block();
                if !self.plain {
                    let level = name.as_bytes()[1] - b'0';
                    for _ in 0..level {
                        self.out.push('#');
                    }
                    self.out.push(' ');
                }
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => self.block(),
            ("pre", false) => {
                self.block();
                if !self.plain {
                    self.out.push_str("```");
                    self.fence_at = Some(self.out.len());
                    self.out.push('\n');
                }
                self.pre += 1;
            }
            ("pre", true) if self.pre > 0 => {
                self.pre -= 1;
                if !self.plain {
                    if !self.out.ends_with('\n') {
                        self.out.push('\n');
                    }
                    self.out.push_str("```");
                }
                self.fence_at = None;
                self.block();
            }
            ("code", false) if self.pre > 0 => {
                // Fence language from `class="language-x"`, only right after the fence opens.
                if self.fence_at.map(|p| p + 1) == Some(self.out.len()) {
                    if let Some(lang) = attr(t.attrs, "class").and_then(|c| {
                        c.split_whitespace()
                            .find_map(|w| w.strip_prefix("language-").map(str::to_string))
                    }) {
                        let at = self.fence_at.unwrap_or(0);
                        self.out.insert_str(at, &lang);
                    }
                }
                self.fence_at = None;
            }
            ("code", _) if self.pre > 0 => {}
            ("code", _) if self.plain => {}
            ("code", _) => self.out.push('`'),
            ("li", false) => {
                self.line();
                self.out.push_str("- ");
            }
            ("br", _) | ("tr", false) | ("dt", false) | ("dd", false) => self.line(),
            ("td" | "th", false) => {
                if !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
                    self.out.push(' ');
                }
            }
            ("a", false) => {
                let href = attr(t.attrs, "href").and_then(|h| self.resolve(&h));
                self.links.push((self.out.len(), href));
            }
            ("a", true) => self.close_link(),
            (n, _) if BLOCK.contains(&n) => self.block(),
            _ => {}
        }
    }

    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        let lower = href.to_ascii_lowercase();
        if href.is_empty() || href.starts_with('#') || lower.starts_with("javascript:") {
            return None;
        }
        match &self.base {
            Some(b) => b.join(href).ok().map(|u| u.to_string()),
            None => Some(href.to_string()),
        }
    }

    fn close_link(&mut self) {
        let Some((start, href)) = self.links.pop() else {
            return;
        };
        if start > self.out.len() || !self.out.is_char_boundary(start) {
            return;
        }
        let inner = self.out[start..]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let lead = if self.out[start..].starts_with(' ') {
            " "
        } else {
            ""
        };
        self.out.truncate(start);
        match href {
            Some(h) if !inner.is_empty() && !self.plain => {
                self.out.push_str(lead);
                self.out.push_str(&format!("[{}]({})", inner, h));
            }
            _ => {
                self.out.push_str(lead);
                self.out.push_str(&inner);
            }
        }
    }
}

enum Markup<'a> {
    Tag(Tag<'a>, usize),
    Comment(usize),
    Text,
}

/// `s` starts with `<`. Returns what it is and how many bytes it spans.
fn parse_tag(s: &str) -> Markup<'_> {
    if let Some(body) = s.strip_prefix("<!--") {
        let len = body.find("-->").map(|e| 4 + e + 3).unwrap_or(s.len());
        return Markup::Comment(len);
    }
    if s.starts_with("<!") || s.starts_with("<?") {
        return Markup::Comment(s.find('>').map(|e| e + 1).unwrap_or(s.len()));
    }
    let closing = s.as_bytes().get(1) == Some(&b'/');
    let name_start = if closing { 2 } else { 1 };
    if !s[name_start..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Markup::Text;
    }
    let name_len = s[name_start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(s.len() - name_start);
    let name = s[name_start..name_start + name_len].to_ascii_lowercase();
    // Find the closing `>`, ignoring any inside quoted attribute values.
    let mut quote: Option<u8> = None;
    let mut end = s.len();
    for (k, b) in s.bytes().enumerate().skip(name_start + name_len) {
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => {
                end = k + 1;
                break;
            }
            _ => {}
        }
    }
    let inner_end = if s[..end].ends_with('>') {
        end - 1
    } else {
        end
    };
    let attrs = &s[name_start + name_len..inner_end];
    Markup::Tag(
        Tag {
            name,
            closing,
            self_closing: attrs.trim_end().ends_with('/'),
            attrs,
        },
        end,
    )
}

/// Bytes to skip past `</name>` in raw-text content (script, style).
fn skip_raw(s: &str, name: &str) -> usize {
    let needle = format!("</{}", name);
    let lower = s.to_ascii_lowercase();
    match lower.find(&needle) {
        Some(p) => p + s[p..].find('>').map(|e| e + 1).unwrap_or(s.len() - p),
        None => s.len(),
    }
}

/// Bytes to skip past the close of a `name` subtree, counting nested opens.
fn skip_subtree(s: &str, name: &str) -> usize {
    let mut depth = 1usize;
    let mut i = 0;
    while let Some(lt) = s[i..].find('<') {
        i += lt;
        match parse_tag(&s[i..]) {
            Markup::Comment(len) => i += len,
            Markup::Text => i += 1,
            Markup::Tag(t, len) => {
                i += len;
                if RAW_TEXT.contains(&t.name.as_str()) && !t.closing {
                    i += skip_raw(&s[i..], &t.name);
                } else if t.name == name {
                    if t.closing {
                        depth -= 1;
                        if depth == 0 {
                            return i;
                        }
                    } else if !t.self_closing {
                        depth += 1;
                    }
                }
            }
        }
    }
    s.len()
}

fn attr(attrs: &str, want: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let name = &attrs[ns..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = None;
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let ve = attrs[vs..]
                    .find(q as char)
                    .map(|e| vs + e)
                    .unwrap_or(b.len());
                value = Some(&attrs[vs..ve]);
                i = (ve + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = Some(&attrs[vs..i]);
            }
        }
        if name.eq_ignore_ascii_case(want) {
            return value.map(decode_entities);
        }
        if ns == i {
            i += 1;
        }
    }
    None
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let semi = rest[..rest.len().min(12)].find(';');
        let decoded = semi.and_then(|e| entity(&rest[1..e]).map(|c| (c, e + 1)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "middot" => '·',
        "bull" => '•',
        "times" => '×',
        "rarr" => '→',
        "larr" => '←',
        _ => return None,
    })
}

fn trim_trailing_spaces(s: &mut String) {
    let keep = s.trim_end_matches([' ', '\t']).len();
    s.truncate(keep);
}

/// Strip trailing spaces per line and collapse runs of blank lines.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &str = include_str!("../../tests/fixtures/web/book_vectors.html");
    const BLOG: &str = include_str!("../../tests/fixtures/web/blog_1_85.html");
    const LEARN: &str = include_str!("../../tests/fixtures/web/rust_learn.html");

    fn md(html: &str) -> String {
        html_to_markdown(html, Some("https://ex.org/a/b"))
    }

    #[test]
    fn drops_script_style_nav_footer() {
        let out = md(
            "<html><head><title>T</title><style>.x{SECRET_STYLE}</style></head>\
             <body><nav><ul><li>SECRET_NAV</li></ul></nav>\
             <script>var a = '</div>SECRET_SCRIPT';</script>\
             <p>Visible text</p><footer><p>SECRET_FOOTER</p></footer></body></html>",
        );
        assert!(out.contains("Visible text"), "{out}");
        for s in [
            "SECRET_STYLE",
            "SECRET_NAV",
            "SECRET_SCRIPT",
            "SECRET_FOOTER",
            "<p>",
        ] {
            assert!(!out.contains(s), "{s} leaked: {out}");
        }
    }

    #[test]
    fn keeps_headings_links_and_code() {
        let out = md("<h2>Install</h2><p>See <a href=\"/docs\">the docs</a>, \
             <a href=\"#top\">top</a> and <code>cargo build</code>.</p>\
             <pre><code class=\"language-sh\">cargo  run\n  --release</code></pre>");
        assert!(out.contains("## Install"), "{out}");
        assert!(out.contains("[the docs](https://ex.org/docs)"), "{out}");
        assert!(
            out.contains(" top and "),
            "fragment link stays plain: {out}"
        );
        assert!(out.contains("`cargo build`"), "{out}");
        assert!(out.contains("```sh\ncargo  run\n  --release\n```"), "{out}");
    }

    #[test]
    fn decodes_entities() {
        let out = md("<p>a &amp; b &lt;c&gt; &#233; &#x41;&nbsp;z</p>");
        assert_eq!(out.trim(), "a & b <c> é A z");
    }

    #[test]
    fn window_bounds_the_text_and_names_the_next_offset() {
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let w = window(&text, 0, 1_000);
        assert!(w.body.len() <= 1_200, "len {}", w.body.len());
        let next = w.next_offset.expect("cut");
        assert!(next <= 1_000 && text[..next].ends_with('\n'), "{next}");
        assert!(w.body.contains(&format!("offset: {next}")), "{}", w.body);
        assert_eq!(w.total, text.len());

        let w2 = window(&text, next, 1_000);
        assert!(w2.body.starts_with(&text[next..next + 10]), "{}", w2.body);

        let tail = window(&text, text.len() - 5, 1_000);
        assert_eq!(tail.next_offset, None);
        assert!(!tail.body.contains("truncated"), "{}", tail.body);
        assert_eq!(window(&text, text.len() + 9, 1_000).body, "");
    }

    #[test]
    fn window_never_splits_a_char() {
        let text = "é".repeat(1_000);
        for off in [0, 1, 3] {
            let w = window(&text, off, 101);
            assert!(w.total == text.len(), "{off}");
        }
    }

    #[test]
    fn text_format_drops_markup() {
        let out = html_to_text(
            "<h2>Install</h2><p>See <a href=\"/docs\">the docs</a> and <code>cargo</code>.</p>\
             <pre><code>run</code></pre>",
            Some("https://ex.org/"),
        );
        assert!(out.contains("Install\n"), "{out}");
        assert!(out.contains("See the docs and cargo."), "{out}");
        for s in ["#", "](", "`", "```"] {
            assert!(!out.contains(s), "{s} in {out}");
        }
        assert!(out.contains("run"), "{out}");
    }

    #[test]
    fn real_pages_shrink_and_keep_their_headings() {
        for (name, html, heading) in [
            (
                "book_vectors",
                BOOK,
                "## Storing Lists of Values with Vectors",
            ),
            ("blog_1_85", BLOG, "## Announcing Rust 1.85.0 and Rust 2024"),
            ("rust_learn", LEARN, "# Learn Rust"),
        ] {
            let out = html_to_markdown(html, Some("https://ex.org/"));
            eprintln!(
                "{name}: html {} B -> markdown {} B ({}%)",
                html.len(),
                out.len(),
                out.len() * 100 / html.len()
            );
            assert!(out.contains(heading), "{name}: missing {heading}");
            assert!(!out.contains("<script"), "{name}");
            assert!(out.len() * 2 < html.len(), "{name}: {} B", out.len());
        }
    }
}
