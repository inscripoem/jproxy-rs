use std::ops::Range;

use quick_xml::escape::{escape, unescape};

/// Count Torznab `<item>` elements without parsing the whole document.
pub fn count_items(xml: &str) -> usize {
    item_ranges(xml).len()
}

/// Append the items from `second` to the channel returned by `first`.
///
/// This follows Java's `XmlUtil.merger`: if the first response has no usable
/// item section, the second response is returned unchanged. Channel metadata
/// and the XML declaration from the first response otherwise win.
pub fn merge_xml(first: &str, second: &str) -> String {
    let first_items = item_ranges(first);
    let second_items = item_ranges(second);
    if first_items.is_empty() || !first.contains("</channel>") {
        return second.to_owned();
    }
    if second_items.is_empty() {
        return first.to_owned();
    }
    let Some(close) = first.rfind("</channel>") else {
        return second.to_owned();
    };
    let second_chunk = &second[second_items[0].start..second_items.last().unwrap().end];
    let mut merged = String::with_capacity(first.len() + second_chunk.len());
    merged.push_str(&first[..close]);
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(second_chunk);
    merged.push('\n');
    merged.push_str("</channel>");
    merged.push_str(&first[close + "</channel>".len()..]);
    merged
}

/// Remove items after the first `limit` entries.
pub fn trim_items(xml: &str, limit: usize) -> String {
    let ranges = item_ranges(xml);
    if ranges.len() <= limit {
        return xml.to_owned();
    }
    let mut trimmed = xml.to_owned();
    for range in ranges.into_iter().skip(limit).rev() {
        trimmed.replace_range(range, "");
    }
    trimmed
}

/// Rewrite only `<item><title>` text nodes. The callback receives the decoded
/// title and the first non-empty `<description>` in the same item.
///
/// Returning `None` leaves an item untouched. When no item is changed the
/// original string is returned unchanged, which is the pass-through path used
/// when no formatting rules exist.
pub fn format_item_titles<F>(xml: &str, mut formatter: F) -> String
where
    F: FnMut(&str, Option<&str>) -> Option<String>,
{
    let mut replacements: Vec<(Range<usize>, String)> = Vec::new();
    for item in item_ranges(xml) {
        let Some(title_range) = element_text_range(xml, item.clone(), "title") else {
            continue;
        };
        let raw_title = &xml[title_range.clone()];
        let decoded_title = decode_xml_text(raw_title);
        let description = element_text_range(xml, item, "description")
            .map(|range| decode_xml_text(&xml[range]))
            .filter(|text| !text.trim().is_empty());
        if let Some(new_title) = formatter(&decoded_title, description.as_deref()) {
            replacements.push((title_range, escape(&new_title).into_owned()));
        }
    }
    if replacements.is_empty() {
        return xml.to_owned();
    }
    let mut output = xml.to_owned();
    for (range, value) in replacements.into_iter().rev() {
        output.replace_range(range, &value);
    }
    output
}

fn decode_xml_text(raw: &str) -> String {
    unescape(raw)
        .map(std::borrow::Cow::into_owned)
        .unwrap_or_else(|_| raw.to_owned())
}

fn item_ranges(xml: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let bytes = xml.as_bytes();
    let mut cursor = 0;
    while let Some(relative) = xml[cursor..].find("<item") {
        let start = cursor + relative;
        let name_end = start + "<item".len();
        let Some(boundary) = bytes.get(name_end).copied() else {
            break;
        };
        if !matches!(boundary, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n') {
            cursor = name_end;
            continue;
        }
        let Some(open_end) = tag_end(xml, name_end) else {
            break;
        };
        if bytes.get(open_end - 2) == Some(&b'/') {
            ranges.push(start..open_end);
            cursor = open_end;
            continue;
        }
        let Some(close_relative) = xml[open_end..].find("</item>") else {
            break;
        };
        let end = open_end + close_relative + "</item>".len();
        ranges.push(start..end);
        cursor = end;
    }
    ranges
}

fn element_text_range(xml: &str, item: Range<usize>, name: &str) -> Option<Range<usize>> {
    let opening = format!("<{name}");
    let relative = xml[item.clone()].find(&opening)?;
    let start = item.start + relative;
    let after_name = start + opening.len();
    let boundary = xml.as_bytes().get(after_name).copied()?;
    if !matches!(boundary, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n') {
        return None;
    }
    let open_end = tag_end(xml, after_name)?;
    if xml.as_bytes().get(open_end - 2) == Some(&b'/') {
        return None;
    }
    let closing = format!("</{name}>");
    let close_relative = xml[open_end..item.end].find(&closing)?;
    let content_end = open_end + close_relative;
    Some(open_end..content_end)
}

fn tag_end(xml: &str, from: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in xml.as_bytes()[from..].iter().copied().enumerate() {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(current), byte) if current == byte => quote = None,
            (None, b'>') => return Some(from + offset + 1),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>Indexer Feed</title>
    <item>
      <title>Show &amp; Tell S01E01 1080p</title>
      <description>Group release</description>
      <link>https://example.invalid/1</link>
    </item>
    <item>
      <title>Second.Release.2024</title>
      <description></description>
    </item>
  </channel>
</rss>
"#;

    #[test]
    fn counts_items_without_counting_channel_title() {
        assert_eq!(count_items(FEED), 2);
        assert_eq!(count_items("<rss><channel></channel></rss>"), 0);
    }

    #[test]
    fn no_op_formatter_preserves_xml_byte_for_byte() {
        let output = format_item_titles(FEED, |_, _| None);
        assert_eq!(output, FEED);
    }

    #[test]
    fn formatting_changes_only_item_titles_and_escapes_text() {
        let output = format_item_titles(FEED, |title, description| {
            if title.starts_with("Show & Tell") {
                assert_eq!(description, Some("Group release"));
                Some("Show & Tell <fixed>".to_owned())
            } else {
                None
            }
        });
        assert!(output.contains("<title>Indexer Feed</title>"));
        assert!(output.contains("<title>Show &amp; Tell &lt;fixed&gt;</title>"));
        assert!(output.contains("<title>Second.Release.2024</title>"));
    }

    #[test]
    fn merge_appends_second_page_items() {
        let second = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Indexer Feed</title>
    <item><title>Third.Release.2024</title></item>
  </channel>
</rss>
"#;
        let merged = merge_xml(FEED, second);
        assert_eq!(count_items(&merged), 3);
        assert!(merged.contains("Indexer Feed"));
        assert!(merged.contains("Show &amp; Tell"));
        assert!(merged.contains("Second.Release.2024"));
        assert!(merged.contains("Third.Release.2024"));
        assert!(merged.starts_with("<?xml"));
    }

    #[test]
    fn trim_keeps_requested_number_of_items() {
        let trimmed = trim_items(FEED, 1);
        assert_eq!(count_items(&trimmed), 1);
        assert!(!trimmed.contains("Second.Release.2024"));
        assert!(trimmed.contains("</channel>"));
    }
}
