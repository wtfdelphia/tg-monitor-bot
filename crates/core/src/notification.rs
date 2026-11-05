//! L3 通知指纹。规范见 `docs/plan/proposals/G4-L3-notify-dedup.md`。

use sha2::{Digest, Sha256};

/// 计算可从 `canonical_events` 重算的版本化通知指纹。
pub fn notification_fingerprint(
    sender_id: Option<i64>,
    matched_keywords: &[String],
    text: Option<&str>,
    caption: Option<&str>,
) -> String {
    let mut keywords = matched_keywords.to_vec();
    keywords.sort_unstable();
    keywords.dedup();
    let keywords = serde_json::to_string(&keywords).unwrap_or_else(|_| "[]".to_owned());
    let body = text
        .filter(|value| !value.is_empty())
        .or_else(|| caption.filter(|value| !value.is_empty()))
        .unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    let raw = format!(
        "tgm:l3:v1\0{}\0{keywords}\0{body}",
        sender_id.map_or_else(String::new, |value| value.to_string())
    );
    Sha256::digest(raw.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::notification_fingerprint;

    #[test]
    fn keyword_order_and_duplicates_do_not_change_fingerprint() {
        let first = notification_fingerprint(
            Some(7),
            &["urgent".to_owned(), "news".to_owned(), "urgent".to_owned()],
            Some("正文"),
            None,
        );
        let second = notification_fingerprint(
            Some(7),
            &["news".to_owned(), "urgent".to_owned()],
            Some("正文"),
            None,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn text_precedes_caption_and_unicode_is_limited_by_scalars() {
        let text = "字".repeat(301);
        let first = notification_fingerprint(Some(7), &[], Some(&text), Some("caption"));
        let second = notification_fingerprint(Some(7), &[], Some(&"字".repeat(300)), None);
        assert_eq!(first, second);
        assert_ne!(
            notification_fingerprint(Some(7), &[], None, Some("caption")),
            notification_fingerprint(Some(7), &[], None, Some("other"))
        );
    }

    #[test]
    fn sender_and_versioned_input_are_part_of_the_fingerprint() {
        assert_ne!(
            notification_fingerprint(Some(7), &[], Some("same"), None),
            notification_fingerprint(Some(8), &[], Some("same"), None)
        );
    }
}
