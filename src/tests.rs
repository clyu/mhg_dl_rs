use super::*;

#[test]
fn test_parse_id() {
    // Test pure numeric ID
    assert_eq!(parse_id("12345"), Some(12345));

    // Test standard web URL
    assert_eq!(parse_id("https://www.manhuagui.com/comic/12345"), Some(12345));
    assert_eq!(parse_id("http://www.manhuagui.com/comic/12345"), Some(12345));

    // Test URL with trailing slash
    assert_eq!(parse_id("https://www.manhuagui.com/comic/12345/"), Some(12345));

    // Test mobile or other subdomain URLs
    assert_eq!(parse_id("https://m.manhuagui.com/comic/12345"), Some(12345));
    assert_eq!(parse_id("https://tw.manhuagui.com/comic/12345"), Some(12345));

    // Test invalid inputs
    assert_eq!(parse_id("https://google.com/comic/12345"), None);
    assert_eq!(parse_id("abcde"), None);
    assert_eq!(parse_id(""), None);
}
