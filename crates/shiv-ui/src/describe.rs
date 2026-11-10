//! Mod descriptions to readable plain text.
//!
//! The catalog's descriptions are GameBanana HTML (`<br>`, lists, links,
//! spans, headings, embedded videos), a few are plain text with newlines and
//! a handful carry BBCode. The detail page shows text only: tags are dropped,
//! block elements become line breaks or paragraphs, list items get a bullet,
//! entities are decoded, and scripts, styles, embeds and images disappear with
//! their content. Link targets are never shown; the link text stays.

/// Longest description shown, in characters. The longest in the live catalog
/// is about 40,000 bytes of markup; a cap keeps the wrapped text cheap to lay
/// out on the software renderer.
pub const MAX_CHARS: usize = 12_000;

/// Elements whose content is dropped along with the tags.
const SKIP_CONTENT: [&str; 9] = [
    "script", "style", "iframe", "noscript", "template", "head", "title", "object", "svg",
];
/// Elements that end the current line.
const LINE_BREAK: [&str; 11] = [
    "div", "tr", "dd", "dt", "pre", "table", "section", "article", "ul", "ol", "dl",
];
/// Elements that stand as their own paragraph.
const PARAGRAPH: [&str; 9] = ["p", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "hr"];
/// Longest BBCode `=` value looked for (`[url=...]`, `[color=...]`). Values
/// beyond this (very long URLs) are dropped up to their `]` or first space.
const BB_VALUE_MAX: usize = 256;
/// Bytes searched for a BBCode tag's `]`: `[`, `/`, a name, `=`, the value, `]`.
const BB_WINDOW: usize = 1 + 1 + 8 + 1 + BB_VALUE_MAX + 1;
/// Bytes searched for an entity's `;` (`&thinsp;`, `&#x10FFFF;`).
const ENTITY_WINDOW: usize = 12;
/// BBCode tags that are recognised (others, like `[WIP]`, stay as text).
const BBCODE: [&str; 17] = [
    "b", "i", "u", "s", "url", "img", "list", "*", "quote", "size", "color", "center", "left",
    "right", "spoiler", "code", "font",
];

/// Plain text for a description: never contains markup, at most
/// [`MAX_CHARS`] characters (plus an ellipsis when cut).
pub fn to_plain_text(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let html = looks_like_html(raw);
    let mut out = Out::default();
    let mut i = 0;
    while i < raw.len() && out.chars <= MAX_CHARS {
        let rest = &raw[i..];
        let Some(c) = rest.chars().next() else { break };
        match c {
            '<' => {
                if let Some(next) = html_tag(raw, &lower, i, &mut out) {
                    i = next;
                    continue;
                }
                out.push('<');
            }
            '[' => {
                if let Some(next) = bbcode_tag(raw, &lower, i, &mut out) {
                    i = next;
                    continue;
                }
                out.push('[');
            }
            '&' => {
                if let Some((decoded, len)) = entity(rest) {
                    if let Some(d) = decoded {
                        out.text_char(d, html);
                    }
                    i += len;
                    continue;
                }
                out.push('&');
            }
            _ => out.text_char(c, html),
        }
        i += c.len_utf8();
    }
    out.finish()
}

/// True when the text uses HTML: then raw newlines are just whitespace.
fn looks_like_html(raw: &str) -> bool {
    let b = raw.as_bytes();
    b.windows(2).enumerate().any(|(i, w)| {
        w[0] == b'<'
            && (w[1].is_ascii_alphabetic()
                || (w[1] == b'/' && b.get(i + 2).is_some_and(u8::is_ascii_alphabetic)))
    })
}

#[derive(Default)]
struct Out {
    s: String,
    chars: usize,
    pending_space: bool,
}

impl Out {
    fn trailing_newlines(&self) -> usize {
        self.s.bytes().rev().take_while(|b| *b == b'\n').count()
    }

    /// A visible character; a pending space is written first, except at the
    /// start of a line.
    fn push(&mut self, c: char) {
        if self.pending_space && !self.s.is_empty() && !self.s.ends_with('\n') {
            self.s.push(' ');
            self.chars += 1;
        }
        self.pending_space = false;
        self.s.push(c);
        self.chars += 1;
    }

    fn text_char(&mut self, c: char, html: bool) {
        match c {
            '\n' if !html => self.line_break(),
            ' ' | '\t' | '\n' | '\r' | '\u{a0}' | '\u{2007}' | '\u{202f}' => {
                self.pending_space = true;
            }
            c if c.is_control() || is_format_char(c) => {}
            c => self.push(c),
        }
    }

    /// `<br>`: one more newline, at most a blank line in a row.
    fn line_break(&mut self) {
        self.pending_space = false;
        if !self.s.is_empty() && self.trailing_newlines() < 2 {
            self.s.push('\n');
            self.chars += 1;
        }
    }

    /// End the line unless it already ended.
    fn end_line(&mut self) {
        if self.trailing_newlines() == 0 {
            self.line_break();
        }
        self.pending_space = false;
    }

    /// A blank line before what follows.
    fn paragraph(&mut self) {
        while !self.s.is_empty() && self.trailing_newlines() < 2 {
            self.line_break();
        }
        self.pending_space = false;
    }

    fn bullet(&mut self) {
        self.end_line();
        self.push('\u{2022}');
        self.pending_space = true;
    }

    fn finish(mut self) -> String {
        let end = self.s.trim_end().len();
        self.s.truncate(end);
        if self.chars <= MAX_CHARS {
            return self.s;
        }
        let cut = self
            .s
            .char_indices()
            .nth(MAX_CHARS)
            .map_or(self.s.len(), |(i, _)| i);
        let head = &self.s[..cut];
        // Prefer ending at a word boundary near the limit.
        let head = match head.rfind(char::is_whitespace) {
            Some(ws) if cut - ws < 200 => &head[..ws],
            _ => head,
        };
        format!("{}\u{2026}", head.trim_end())
    }
}

/// Handle an HTML tag, comment or declaration at `i` (a `<`). Returns the
/// index after it, or `None` when the `<` is plain text (`a < b`).
fn html_tag(raw: &str, lower: &str, i: usize, out: &mut Out) -> Option<usize> {
    let rest = &lower[i..];
    if rest.starts_with("<!--") {
        return Some(rest.find("-->").map_or(raw.len(), |e| i + e + 3));
    }
    let b = rest.as_bytes();
    let (closing, name_at) = match b.get(1) {
        Some(b'/') => (true, 2),
        Some(b'!' | b'?') => {
            return Some(rest.find('>').map_or(raw.len(), |e| i + e + 1));
        }
        _ => (false, 1),
    };
    if !b.get(name_at).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    // A tag cut off at the end of the text is dropped with what follows.
    let Some(len) = tag_end(rest) else {
        return Some(raw.len());
    };
    let end = i + len;
    let name_len = b[name_at..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    let name = &rest[name_at..name_at + name_len];
    if !closing && SKIP_CONTENT.contains(&name) {
        let close = format!("</{name}");
        return Some(match lower[end..].find(&close) {
            Some(at) => tag_end(&lower[end + at..]).map_or(raw.len(), |e| end + at + e),
            None => raw.len(),
        });
    }
    match name {
        "br" => out.line_break(),
        "li" if !closing => out.bullet(),
        "li" => out.end_line(),
        n if PARAGRAPH.contains(&n) => out.paragraph(),
        n if LINE_BREAK.contains(&n) => out.end_line(),
        "td" | "th" if closing => out.pending_space = true,
        _ => {}
    }
    Some(end)
}

/// Unicode format characters (category Cf): bidi overrides and isolates,
/// zero-width characters, soft hyphen, BOM and tag characters. Invisible,
/// but able to reorder or hide the text around them.
fn is_format_char(c: char) -> bool {
    matches!(c,
        '\u{ad}'
        | '\u{600}'..='\u{605}'
        | '\u{61c}'
        | '\u{6dd}'
        | '\u{70f}'
        | '\u{180e}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}'
        | '\u{feff}'
        | '\u{fff9}'..='\u{fffb}'
        | '\u{110bd}'
        | '\u{1d173}'..='\u{1d17a}'
        | '\u{e0001}'
        | '\u{e0020}'..='\u{e007f}')
}

/// Length of a tag up to and including its `>`, skipping `>` inside quoted
/// attribute values. A quote only opens a value right after `=` (spaces
/// allowed), so an apostrophe in `<a title=it's>` or in text after a stray
/// `<b` does not swallow what follows. `None` when the tag never closes.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    let mut after_eq = false;
    for (at, c) in tag.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') if after_eq => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '>') => return Some(at + 1),
            (None, '=') => after_eq = true,
            (None, c) if c.is_whitespace() => {}
            (None, _) => after_eq = false,
        }
    }
    None
}

/// Handle a known BBCode tag at `i` (a `[`). `[img]...[/img]` drops its URL.
/// Every search is bounded or consumes what it scanned, so the whole
/// conversion stays linear however many `[` the text has.
fn bbcode_tag(raw: &str, lower: &str, i: usize, out: &mut Out) -> Option<usize> {
    let rest = &lower[i..];
    let b = rest.as_bytes();
    let window = &b[..b.len().min(BB_WINDOW)];
    let (name, closing, end) = match window.iter().position(|c| *c == b']') {
        Some(close) => {
            // `]` is ASCII, so `close` is a character boundary.
            let inner = &rest[1..close];
            let (closing, inner) = match inner.strip_prefix('/') {
                Some(s) => (true, s),
                None => (false, inner),
            };
            let name = inner.split('=').next().unwrap_or(inner);
            if !BBCODE.contains(&name) || (closing && inner.contains('=')) {
                return None;
            }
            (name, closing, i + close + 1)
        }
        None => {
            // No `]` close by: a known `[name=` with an overlong value (a long
            // URL) is dropped up to its `]`, or up to the first whitespace when
            // it never closes. A short value is not a tag.
            let name_len = b[1..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count();
            let name = &rest[1..1 + name_len];
            if !BBCODE.contains(&name) || b.get(1 + name_len) != Some(&b'=') {
                return None;
            }
            let value = &b[2 + name_len..];
            let stop = value
                .iter()
                .position(|c| *c == b']' || c.is_ascii_whitespace())
                .unwrap_or(value.len());
            if stop <= BB_VALUE_MAX {
                return None;
            }
            let closed = usize::from(value.get(stop) == Some(&b']'));
            (name, false, i + 2 + name_len + stop + closed)
        }
    };
    match name {
        "img" if !closing => {
            return Some(
                lower[end..]
                    .find("[/img]")
                    .map_or(raw.len(), |at| end + at + 6),
            );
        }
        "*" => out.bullet(),
        "list" | "quote" | "center" | "left" | "right" | "code" => out.end_line(),
        _ => {}
    }
    Some(end)
}

/// Decode an entity at the start of `s`: `(Some(char), len)` for a known one,
/// `(None, len)` for one that decodes to nothing usable, `None` for text that
/// is not an entity (a lone `&`).
fn entity(s: &str) -> Option<(Option<char>, usize)> {
    let b = s.as_bytes();
    // A byte search: `;` is ASCII, so the index is a character boundary.
    let semi = b[..b.len().min(ENTITY_WINDOW)]
        .iter()
        .position(|c| *c == b';')?;
    let body = &s[1..semi];
    let len = semi + 1;
    if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        let c = char::from_u32(code).filter(|c| *c != '\0');
        return Some((c, len));
    }
    let c = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" | "ensp" | "emsp" | "thinsp" => ' ',
        "middot" => '\u{b7}',
        "bull" => '\u{2022}',
        "hellip" => '\u{2026}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "rarr" => '\u{2192}',
        "larr" => '\u{2190}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "deg" => '\u{b0}',
        "times" => '\u{d7}',
        _ => return None,
    };
    Some((Some(c), len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaks_paragraphs_and_lists() {
        let raw = "Old money. New drip.<br><br>Mod includes:<br>- Outfits;<br>\n- Effects<br><br>";
        assert_eq!(
            to_plain_text(raw),
            "Old money. New drip.\n\nMod includes:\n- Outfits;\n- Effects"
        );
        let list = "Changed:<br><ul data-spread=\"false\"><li>Hero mode<br></li><li>Icons</li></ul><br>Done";
        assert_eq!(
            to_plain_text(list),
            "Changed:\n\u{2022} Hero mode\n\u{2022} Icons\n\nDone"
        );
        assert_eq!(
            to_plain_text("<h1>Title</h1><p>One</p><p>Two</p><hr>End"),
            "Title\n\nOne\n\nTwo\n\nEnd"
        );
        // No more than one blank line in a row.
        assert_eq!(to_plain_text("a<br><br><br><br>b"), "a\n\nb");
    }

    #[test]
    fn strips_tags_and_keeps_link_text() {
        let raw = r#"By <a class="MemberLink" href="https://gamebanana.com/members/1">ruineii</a>. <span class="RedColor">Do not </span>combine. <b>Bold</b> <font color="red">red</font><img src="x.png" alt="pic">"#;
        let text = to_plain_text(raw);
        assert_eq!(text, "By ruineii. Do not combine. Bold red");
        assert!(!text.contains("href") && !text.contains("gamebanana.com"));
        // A `>` inside an attribute does not end the tag.
        assert_eq!(to_plain_text(r#"<a title="a>b" href="x">Link</a>"#), "Link");
    }

    #[test]
    fn drops_scripts_styles_and_embeds() {
        let raw = r#"<h1><iframe width="560" src="https://www.youtube.com/embed/x"></iframe></h1><br><i>Ported voices</i><script>alert("<b>hi</b>")</script><STYLE>p{color:red}</STYLE> done<!-- note <b> -->!"#;
        let text = to_plain_text(raw);
        assert_eq!(text, "Ported voices done!");
        // An unclosed script swallows the rest rather than showing code.
        assert_eq!(to_plain_text("Keep<script>var a = 1;"), "Keep");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(
            to_plain_text(
                "Tom &amp; Jerry&nbsp;&lt;3 &quot;x&quot; &#39;y&#39; &#x2014; &middot; &rarr;"
            ),
            "Tom & Jerry <3 \"x\" 'y' \u{2014} \u{b7} \u{2192}"
        );
        // Unknown or broken entities stay as text; a decoded `<` is not a tag.
        assert_eq!(to_plain_text("a &bogus; b & c &#0; d"), "a &bogus; b & c d");
        assert_eq!(to_plain_text("&lt;b&gt;bold&lt;/b&gt;"), "<b>bold</b>");
    }

    #[test]
    fn plain_text_keeps_its_newlines() {
        assert_eq!(
            to_plain_text("Line one\r\nLine two\n\n\n\nLast  word\t!"),
            "Line one\nLine two\n\nLast word !"
        );
        assert_eq!(to_plain_text("a < b and c > d"), "a < b and c > d");
        assert_eq!(to_plain_text("   "), "");
        assert_eq!(to_plain_text(""), "");
    }

    #[test]
    fn bbcode_is_stripped_but_brackets_stay() {
        let raw = "[b]Bold[/b] [url=https://x.test]site[/url] [img]https://x.test/a.png[/img][list][*]one[*]two[/list] [WIP]";
        assert_eq!(
            to_plain_text(raw),
            "Bold site\n\u{2022} one\n\u{2022} two\n[WIP]"
        );
    }

    #[test]
    fn very_long_text_is_cut_at_a_word() {
        let raw = "word ".repeat(10_000) + "<br>tail";
        let text = to_plain_text(&raw);
        assert!(text.ends_with('\u{2026}'));
        assert!(text.chars().count() <= MAX_CHARS + 1);
        assert!(text.chars().count() > MAX_CHARS - 200);
        assert!(!text.contains("tail"));
        assert!(text.trim_end_matches('\u{2026}').ends_with("word"));
        // Huge input without spaces: cut at the limit, still bounded.
        let blob = "x".repeat(MAX_CHARS * 50);
        assert_eq!(to_plain_text(&blob).chars().count(), MAX_CHARS + 1);
        // Multi-byte characters are cut on a character boundary.
        let wide = "\u{e9}".repeat(MAX_CHARS + 10);
        assert_eq!(to_plain_text(&wide).chars().count(), MAX_CHARS + 1);
    }

    #[test]
    fn multibyte_text_near_brackets_and_ampersands() {
        // The search windows end inside a multi-byte character.
        let e = "\u{e9}".repeat(40);
        assert_eq!(to_plain_text(&format!("a &{e};")), format!("a &{e};"));
        assert_eq!(to_plain_text(&format!("a [{e}]")), format!("a [{e}]"));
        assert_eq!(
            to_plain_text("&amp;\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"),
            "&\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"
        );
        let pad = "x".repeat(super::BB_WINDOW - 4);
        assert_eq!(
            to_plain_text(&format!("[b{pad}\u{e9}\u{e9}]z")),
            format!("[b{pad}\u{e9}\u{e9}]z")
        );
    }

    #[test]
    fn long_bbcode_values() {
        let url = format!("https://x.test/{}", "a".repeat(150));
        assert_eq!(
            to_plain_text(&format!("[url={url}]site[/url] end")),
            "site end"
        );
        // Longer than the window: dropped up to its `]`.
        let long = format!("https://x.test/{}", "b".repeat(1000));
        assert_eq!(
            to_plain_text(&format!("go [url={long}]site[/url] now")),
            "go site now"
        );
        // Never closed: dropped up to the first space, the rest stays.
        assert_eq!(to_plain_text(&format!("go [url={long} now")), "go now");
        // A short unclosed value and unknown names are plain text.
        assert_eq!(to_plain_text("[size=3 big"), "[size=3 big");
        assert_eq!(
            to_plain_text(&format!("[note={long}]")),
            format!("[note={long}]")
        );
    }

    #[test]
    fn apostrophes_do_not_swallow_text() {
        assert_eq!(to_plain_text("<a title=it's>Link</a> after"), "Link after");
        assert_eq!(
            to_plain_text("I <3 it's <b>fine</b>, isn't it"),
            "I <3 it's fine, isn't it"
        );
        assert_eq!(to_plain_text("<a href = 'x>y'>ok</a>"), "ok");
    }

    #[test]
    fn format_characters_are_removed() {
        let raw =
            "safe\u{202e}txt.exe\u{202c} a\u{200b}b \u{2066}x\u{2069} &#x202E;y \u{feff}z\u{ad}";
        assert_eq!(to_plain_text(raw), "safetxt.exe ab x y z");
    }

    /// Many `[` and `&` with multi-byte text around them stay linear. The
    /// first inputs were quadratic before (seconds); the last one is all
    /// markup that is consumed silently, so every byte is processed, and
    /// gets a looser bound for unoptimised test builds under load.
    #[test]
    fn large_inputs_stay_fast() {
        let ms = std::time::Duration::from_millis;
        let inputs = [
            // Five-byte units, so the old fixed windows ended mid-character.
            ("&\u{e9}\u{e9}".repeat(400_000), ms(200)),
            ("[\u{e9}\u{e9}".repeat(400_000), ms(200)),
            ("[url=\u{e9}".repeat(300_000), ms(200)),
            (
                format!("[url=https://x.test/{}]", "c".repeat(300)).repeat(6_000),
                ms(200),
            ),
            ("&#0;[b][/b]<i></i>".repeat(110_000), ms(1000)),
        ];
        for (raw, limit) in &inputs {
            assert!(raw.len() >= 1_900_000, "{}", raw.len());
            let start = std::time::Instant::now();
            let text = to_plain_text(raw);
            let took = start.elapsed();
            assert!(text.chars().count() <= MAX_CHARS + 1);
            assert!(took < *limit, "{took:?} for {:?}...", &raw[..20]);
        }
    }

    #[test]
    fn never_shows_markup() {
        let raw = "<div><table><tr><td>a</td><td>b</td></tr></table></div><input type=checkbox><code>c</code><unclosed";
        let text = to_plain_text(raw);
        assert_eq!(text, "a b\nc");
    }
}
