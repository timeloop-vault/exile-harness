//! Rendered-HTML → readable text conversion.
//!
//! The wikis' hard data (item stats, penalty tables, gem numbers) is
//! largely *template-generated*: raw wikitext contains `{{templates}}`
//! whose expansion holds the actual content. So the tool fetches the
//! rendered HTML (`action=parse&prop=text`) and converts that — a lesson
//! from an earlier scraping project where stripping wikitext templates
//! silently deleted the data. Block elements become line breaks, table
//! cells get separators, script/style/comments are dropped.

/// Convert rendered `MediaWiki` HTML into readable plain text.
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];

        // Comments are handled inside the walk (not as a pre-pass) so a
        // `<!--` inside script content cannot swallow page text.
        if after.starts_with("<!--") {
            if let Some(end) = after.find("-->") {
                rest = &after[end + 3..];
            } else {
                rest = "";
            }
            continue;
        }
        // HTML5 tokenizer rule: `<` not followed by a letter, `/` or `!`
        // is plain text, not a tag opener.
        if !matches!(
            after[1..].chars().next(),
            Some(c) if c.is_ascii_alphabetic() || c == '/' || c == '!'
        ) {
            out.push('<');
            rest = &after[1..];
            continue;
        }
        let Some(end) = after.find('>') else {
            // Trailing partial tag: drop it.
            rest = "";
            break;
        };
        let tag_body = &after[1..end];
        let closing = tag_body.starts_with('/');
        let name = tag_name(tag_body);

        // script/style contents are never text.
        if !closing && (name == "script" || name == "style") {
            let close_marker = format!("</{name}");
            let Some(close_start) = after[end..].find(&close_marker) else {
                rest = "";
                break;
            };
            let from_close = &after[end + close_start..];
            let Some(close_end) = from_close.find('>') else {
                rest = "";
                break;
            };
            rest = &from_close[close_end + 1..];
            continue;
        }

        match name {
            "br" | "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr" | "hr"
            | "table" | "ul" | "ol" => out.push('\n'),
            "td" | "th" if !closing => out.push_str(" | "),
            _ => {}
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    collapse_blank_lines(&decode_entities(&out))
}

/// Clean a search-result snippet (HTML with match markers) into text.
pub(crate) fn clean_snippet(snippet: &str) -> String {
    let mut out = String::with_capacity(snippet.len());
    let mut rest = snippet;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if !matches!(
            after[1..].chars().next(),
            Some(c) if c.is_ascii_alphabetic() || c == '/' || c == '!'
        ) {
            out.push('<');
            rest = &after[1..];
            continue;
        }
        let Some(end) = after.find('>') else {
            rest = "";
            break;
        };
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

/// The element name of a tag body like `p class="x"`, `/p`, or `br/`.
fn tag_name(tag_body: &str) -> &str {
    let trimmed = tag_body.trim_start_matches('/');
    let end = trimmed
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(trimmed.len());
    &trimmed[..end]
}

/// Decode numeric character references (`&#91;`, `&#x2212;`) — the live
/// wikis emit these inside formulas and reference brackets.
fn decode_numeric_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("&#") {
        out.push_str(&rest[..start]);
        let body = &rest[start + 2..];
        let Some(semi) = body.find(';').filter(|&i| i > 0 && i <= 8) else {
            out.push_str("&#");
            rest = body;
            continue;
        };
        let code = &body[..semi];
        let value = if let Some(hex) = code.strip_prefix('x').or_else(|| code.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            code.parse::<u32>().ok()
        };
        if let Some(ch) = value.and_then(char::from_u32) {
            out.push(ch);
            rest = &body[semi + 1..];
        } else {
            out.push_str("&#");
            rest = body;
        }
    }
    out.push_str(rest);
    out
}

fn decode_entities(text: &str) -> String {
    decode_numeric_entities(text)
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace('\u{a0}', " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Trim line ends and collapse runs of blank lines to a single one.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line.trim_start());
        out.push('\n');
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_generated_table_data_survives() {
        let html = r#"<div class="mw-parser-output"><table class="wikitable"><tr><th>Boss</th><th>Penalty</th></tr><tr><td>Act 5</td><td>-30%</td></tr><tr><td>Act 10</td><td>-30%</td></tr></table></div>"#;
        let text = html_to_text(html);
        assert!(text.contains("| Boss | Penalty"));
        assert!(text.contains("| Act 5 | -30%"));
        assert!(text.contains("| Act 10 | -30%"));
    }

    #[test]
    fn block_elements_become_lines_and_scripts_vanish() {
        let html = "<p>First.</p><script>evil()</script><style>.x{}</style><h2>Heading</h2><ul><li>one</li><li>two</li></ul>";
        let text = html_to_text(html);
        assert_eq!(text, "First.\n\nHeading\n\none\n\ntwo");
        assert!(!text.contains("evil"));
    }

    #[test]
    fn comments_and_entities_are_handled() {
        let html = "a<!-- hidden limit report -->b &amp; &quot;c&quot;&#160;d";
        assert_eq!(html_to_text(html), "ab & \"c\" d");
    }

    #[test]
    fn comment_marker_inside_script_cannot_swallow_content() {
        let html = "<p>a</p><script>var s = \"<!--\";</script><p>REAL CONTENT</p>";
        let text = html_to_text(html);
        assert!(text.contains("REAL CONTENT"));
        assert!(!text.contains("var s"));
    }

    #[test]
    fn bare_less_than_is_preserved_as_text() {
        assert_eq!(
            html_to_text("damage < 50 <b>and</b> more"),
            "damage < 50 and more"
        );
        assert_eq!(clean_snippet("a < b"), "a < b");
    }

    #[test]
    fn escaped_entities_stay_literal_not_double_decoded() {
        // `&amp;#x2212;` in HTML source means the literal text "&#x2212;":
        // the author escaped the ampersand so it must NOT decode further.
        // A browser renders the reference itself, and so do we — which is
        // why `&amp;` is decoded last, after the numeric pass.
        assert_eq!(html_to_text("a &amp;#x2212; b"), "a &#x2212; b");
        assert_eq!(html_to_text("a &amp;lt;b&amp;gt; c"), "a &lt;b&gt; c");
    }

    #[test]
    fn numeric_entities_decode() {
        assert_eq!(
            html_to_text("x &#91;1&#93; is &#x2212;5"),
            "x [1] is \u{2212}5"
        );
        // Invalid references stay literal instead of corrupting text.
        assert_eq!(
            html_to_text("a &#zz; b &#123456789; c"),
            "a &#zz; b &#123456789; c"
        );
    }

    #[test]
    fn snippet_cleaning_strips_match_markers() {
        let snippet = r#"the <span class="searchmatch">league</span> mechanic &quot;x&quot;"#;
        assert_eq!(clean_snippet(snippet), "the league mechanic \"x\"");
    }

    #[test]
    fn malformed_html_does_not_panic() {
        let _ = html_to_text("<p unclosed");
        let _ = html_to_text("<script>never closed");
        let _ = html_to_text("<!-- never closed");
        let _ = html_to_text("plain < bare");
    }
}
