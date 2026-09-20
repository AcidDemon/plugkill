//! Colouring and numbering for the output panes (K3).
//!
//! Two small tokenizers, one for TOML and one for Nix, rendered as Pango
//! markup in the labels the panes already use. No syntax crate: it would tie
//! the package to a second GTK versioned dependency for this much code.
//!
//! One colour set serves both grounds the stylesheet handles. The dark well
//! is #15191d and the light one #dfe4e9, so every colour here is a mid tone
//! that keeps at least 3:1 against both; a colour picked for one ground alone
//! would vanish on the other, because Pango markup is absolute and does not
//! follow the .light class.

use gtk::glib;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Toml,
    Nix,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token {
    Comment,
    Key,
    Str,
    Number,
    Bool,
    Punct,
    Plain,
}

impl Token {
    /// The colour, or none for text that keeps the label's own.
    fn colour(self) -> Option<&'static str> {
        match self {
            Token::Comment => Some("#6d777f"),
            Token::Key => Some("#2f7fbf"),
            Token::Str => Some("#2e8a4e"),
            Token::Number => Some("#a8780f"),
            Token::Bool => Some("#8a5cd0"),
            Token::Punct => Some("#7e878f"),
            Token::Plain => None,
        }
    }
}

/// One line as spans. The spans concatenate back to the line exactly, so
/// nothing can be dropped on the way to the pane.
fn tokens(lang: Lang, line: &str) -> Vec<(Token, &str)> {
    let mut out = Vec::new();
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    if !indent.is_empty() {
        out.push((Token::Plain, indent));
    }
    if body.is_empty() {
        return out;
    }
    if body.starts_with('#') {
        out.push((Token::Comment, body));
        return out;
    }
    match assignment(body) {
        // `key = value`, in both languages. What is left of the sign names
        // something, what is right of it is a value.
        Some(at) => {
            scan(&body[..at], true, &mut out);
            out.push((Token::Punct, &body[at..at + 1]));
            scan(&body[at + 1..], false, &mut out);
        }
        // No sign: a TOML section header names something, anything else is
        // a value, a bracket or a brace.
        None => scan(body, lang == Lang::Toml && body.starts_with('['), &mut out),
    }
    out
}

/// The byte offset of the `=` that makes this line an assignment, ignoring
/// one inside a string and one inside a comment.
fn assignment(body: &str) -> Option<usize> {
    let mut i = 0;
    while i < body.len() {
        let rest = &body[i..];
        let c = rest.chars().next()?;
        match c {
            '"' => i += string_end(rest),
            '#' => return None,
            '=' => return (!body[..i].trim().is_empty()).then_some(i),
            _ => i += c.len_utf8(),
        }
    }
    None
}

/// The spans of one stretch of text. With `keys`, a bare word names
/// something; without it, a bare word is a value and may be a boolean.
fn scan<'a>(text: &'a str, keys: bool, out: &mut Vec<(Token, &'a str)>) {
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let Some(c) = rest.chars().next() else { break };
        let step = match c {
            ' ' | '\t' => {
                let n = run(rest, |ch| ch == ' ' || ch == '\t');
                out.push((Token::Plain, &rest[..n]));
                n
            }
            '#' => {
                out.push((Token::Comment, rest));
                rest.len()
            }
            '"' => {
                let n = string_end(rest);
                out.push((Token::Str, &rest[..n]));
                n
            }
            '[' | ']' | '{' | '}' | '(' | ')' | ',' | ';' | '=' | ':' | '.' => {
                out.push((Token::Punct, &rest[..1]));
                1
            }
            _ if c.is_ascii_digit() || (c == '-' && starts_digit(&rest[1..])) => {
                // Dots, colons and signs stay in, so a TOML datetime or a
                // float is one number and not five tokens.
                let n = 1 + run(&rest[1..], |ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-' | ':')
                });
                out.push((Token::Number, &rest[..n]));
                n
            }
            _ if c.is_alphabetic() || c == '_' => {
                let n = run(rest, |ch| ch.is_alphanumeric() || ch == '_' || ch == '-');
                let word = &rest[..n];
                let kind = match (keys, word) {
                    (true, _) => Token::Key,
                    (false, "true" | "false") => Token::Bool,
                    _ => Token::Plain,
                };
                out.push((kind, word));
                n
            }
            _ => {
                let n = c.len_utf8();
                out.push((Token::Plain, &rest[..n]));
                n
            }
        };
        i += step;
    }
}

fn run(text: &str, keep: impl Fn(char) -> bool) -> usize {
    text.find(|ch: char| !keep(ch)).unwrap_or(text.len())
}

fn starts_digit(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_digit())
}

/// The length of the string starting at `rest`, closing quote included. An
/// unterminated string runs to the end of the line rather than swallowing it.
fn string_end(rest: &str) -> usize {
    let mut escaped = false;
    for (i, c) in rest.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => return i + 1,
            _ => {}
        }
    }
    rest.len()
}

/// One line as Pango markup. The text is escaped first, so a path holding an
/// ampersand or an angle bracket neither corrupts the pane nor vanishes.
fn line_markup(lang: Lang, line: &str) -> String {
    let mut out = String::new();
    for (token, text) in tokens(lang, line) {
        let escaped = glib::markup_escape_text(text);
        match token.colour() {
            Some(colour) => {
                out.push_str(&format!("<span foreground=\"{colour}\">{escaped}</span>"))
            }
            None => out.push_str(&escaped),
        }
    }
    out
}

/// A whole document as Pango markup.
pub fn markup(lang: Lang, text: &str) -> String {
    text.lines()
        .map(|line| line_markup(lang, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One diff line: the B5 mark, and the colours inside it, so a changed line
/// stays marked while it is highlighted (K3a). The star is bold and the
/// colour spans sit within the bold, which is the nesting Pango accepts.
pub fn diff_line_markup(lang: Lang, line: &str, changed: bool) -> String {
    let body = line_markup(lang, line);
    if changed {
        format!("<b>* {body}</b>")
    } else {
        format!("  {body}")
    }
}

/// The line number gutter, right aligned to the widest number. It lives in a
/// label of its own beside the code, so a selection never picks it up and the
/// copy button never sees it.
pub fn gutter(lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let width = lines.to_string().len();
    (1..=lines)
        .map(|n| format!("{n:>width$}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lang: Lang, line: &str) -> Vec<(Token, &str)> {
        let spans = tokens(lang, line);
        let joined: String = spans.iter().map(|(_, t)| *t).collect();
        assert_eq!(joined, line, "the spans must rebuild the line");
        spans
    }

    #[test]
    fn test_a_comment_is_one_span() {
        assert_eq!(
            kinds(Lang::Toml, "# what root runs"),
            vec![(Token::Comment, "# what root runs")]
        );
    }

    #[test]
    fn test_an_indented_nix_comment_keeps_its_indent() {
        assert_eq!(
            kinds(Lang::Nix, "  # a note"),
            vec![(Token::Plain, "  "), (Token::Comment, "# a note")]
        );
    }

    #[test]
    fn test_a_key_and_a_string_value() {
        assert_eq!(
            kinds(Lang::Toml, r#"path = "/etc/plugkill.toml""#),
            vec![
                (Token::Key, "path"),
                (Token::Plain, " "),
                (Token::Punct, "="),
                (Token::Plain, " "),
                (Token::Str, r#""/etc/plugkill.toml""#),
            ]
        );
    }

    #[test]
    fn test_a_string_holding_a_quote_stays_one_span() {
        let spans = kinds(Lang::Toml, r#"note = "he said \"hi\" once""#);
        assert_eq!(
            spans.last(),
            Some(&(Token::Str, r#""he said \"hi\" once""#))
        );
    }

    #[test]
    fn test_a_number_value() {
        let spans = kinds(Lang::Toml, "grace_secs = 30");
        assert_eq!(spans.last(), Some(&(Token::Number, "30")));
    }

    #[test]
    fn test_a_negative_number_keeps_its_sign() {
        let spans = kinds(Lang::Nix, "offset = -12;");
        assert!(spans.contains(&(Token::Number, "-12")));
    }

    #[test]
    fn test_a_boolean_value() {
        let spans = kinds(Lang::Toml, "watch = true");
        assert_eq!(spans.last(), Some(&(Token::Bool, "true")));
    }

    #[test]
    fn test_a_boolean_named_as_a_key_is_not_a_boolean() {
        let spans = kinds(Lang::Nix, "true = 1;");
        assert_eq!(spans.first(), Some(&(Token::Key, "true")));
    }

    #[test]
    fn test_an_empty_line_has_no_spans() {
        assert!(tokens(Lang::Toml, "").is_empty());
        assert!(tokens(Lang::Nix, "").is_empty());
    }

    #[test]
    fn test_a_toml_header_names_its_section() {
        assert_eq!(
            kinds(Lang::Toml, "[usb]"),
            vec![
                (Token::Punct, "["),
                (Token::Key, "usb"),
                (Token::Punct, "]"),
            ]
        );
    }

    #[test]
    fn test_a_nix_attrset_opens_with_a_key() {
        let spans = kinds(Lang::Nix, "  usb = {");
        assert!(spans.contains(&(Token::Key, "usb")));
        assert!(spans.contains(&(Token::Punct, "{")));
    }

    #[test]
    fn test_a_nix_list_item_is_a_string() {
        let spans = kinds(Lang::Nix, r#"    "1050:0407""#);
        assert!(spans.contains(&(Token::Str, r#""1050:0407""#)));
    }

    #[test]
    fn test_a_trailing_comment_is_a_comment() {
        let spans = kinds(Lang::Toml, "watch = true # on");
        assert_eq!(spans.last(), Some(&(Token::Comment, "# on")));
    }

    #[test]
    fn test_an_equals_sign_inside_a_string_is_not_an_assignment() {
        assert_eq!(assignment(r#""a=b""#), None);
    }

    #[test]
    fn test_markup_escapes_an_ampersand_and_an_angle_bracket() {
        let out = line_markup(Lang::Toml, r#"path = "/tmp/a&b<c>""#);
        assert!(out.contains("&amp;"), "{out}");
        assert!(out.contains("&lt;c&gt;"), "{out}");
        assert!(!out.contains("a&b"), "{out}");
    }

    #[test]
    fn test_markup_keeps_every_character_of_the_line() {
        let line = "shred = \"/home/a&b/<x>\"";
        let out = line_markup(Lang::Toml, line);
        let stripped = strip(&out);
        assert_eq!(stripped, line);
    }

    /// Undoes the markup, so a test can check nothing was lost.
    fn strip(markup: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in markup.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    #[test]
    fn test_markup_of_a_document_has_one_line_per_line() {
        let out = markup(Lang::Toml, "[usb]\nwatch = true\n");
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn test_the_gutter_numbers_one_line() {
        assert_eq!(gutter(1), "1");
    }

    #[test]
    fn test_the_gutter_right_aligns_a_many_line_document() {
        let out = gutter(12);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[0], " 1");
        assert_eq!(lines[8], " 9");
        assert_eq!(lines[11], "12");
    }

    #[test]
    fn test_a_changed_diff_line_keeps_its_mark_and_its_colours() {
        let marked = diff_line_markup(Lang::Toml, "key = \"v\"", true);
        assert!(marked.starts_with("<b>* "), "{marked}");
        assert!(marked.ends_with("</b>"), "{marked}");
        let key = line_markup(Lang::Toml, "key = \"v\"");
        assert!(key.contains("<span"), "the line is coloured at all");
        // The colour spans sit inside the bold, which is what Pango takes.
        assert!(marked.contains(&key), "{marked}");
        assert_eq!(strip(&marked), "* key = \"v\"");

        let plain = diff_line_markup(Lang::Toml, "key = \"v\"", false);
        assert!(!plain.contains("<b>"), "{plain}");
        assert_eq!(strip(&plain), "  key = \"v\"");
    }

    #[test]
    fn test_the_gutter_of_nothing_is_nothing() {
        assert_eq!(gutter(0), "");
    }

    #[test]
    fn test_the_numbers_live_only_in_the_gutter() {
        let document = "[usb]\nwatch = true";
        let code = markup(Lang::Toml, document);
        let numbers = gutter(document.lines().count());
        // The copy button is handed the document itself. The markup and the
        // numbers are made from it and never mixed into it, so the clipboard
        // gets the config alone.
        assert_eq!(strip(&code), document);
        assert!(!document.contains("<span"));
        assert!(
            numbers
                .chars()
                .all(|c| c.is_ascii_digit() || c == ' ' || c == '\n'),
            "{numbers}"
        );
        assert_eq!(numbers.lines().count(), code.lines().count());
    }
}
