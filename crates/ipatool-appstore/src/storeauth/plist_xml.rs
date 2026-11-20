//! The XML property lists Go's `plist.Marshal(..., plist.XMLFormat)` writes for the
//! provisioning requests: dictionaries of strings, keys sorted, no indentation.

use std::collections::BTreeMap;

pub(super) enum Node<'a> {
    String(&'a str),
    Dict(BTreeMap<&'a str, Node<'a>>),
}

pub(super) fn encode(root: &BTreeMap<&str, Node<'_>>) -> Vec<u8> {
    let mut out = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">"
    ));
    write_dict(&mut out, root);
    out.push_str("</plist>");
    out.into_bytes()
}

fn write_dict(out: &mut String, dict: &BTreeMap<&str, Node<'_>>) {
    out.push_str("<dict>");
    for (key, value) in dict {
        out.push_str("<key>");
        escape(out, key);
        out.push_str("</key>");
        match value {
            Node::String(text) => {
                out.push_str("<string>");
                escape(out, text);
                out.push_str("</string>");
            }
            Node::Dict(dict) => write_dict(out, dict),
        }
    }
    out.push_str("</dict>");
}

/// Go's `xml.EscapeText`, including its U+FFFD for characters XML cannot carry.
fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c if c < ' ' || c == '\u{fffe}' || c == '\u{ffff}' => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
}
