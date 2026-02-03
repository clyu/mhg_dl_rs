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

#[test]
fn test_unpack_packed() {
    // A simplified example of "packed" JavaScript code and its dictionary.
    // Removed spaces between '(' and '{' to match the re_json regex: .*\((\{.*\})\).*
    let frame = "SMH.imgData({\"0\":{\"1\":\"123\",\"2\":\"abc\"},\"3\":\"/comic/\",\"4\":[\"01.jpg\"]})";
    let a = 10;
    let c = 5;
    let data = vec![
        "sl".to_string(),    // 0
        "e".to_string(),     // 1
        "m".to_string(),     // 2
        "path".to_string(),  // 3
        "files".to_string(), // 4
    ];

    let result = unpack_packed(frame, a, c, data).unwrap();

    // Verify the unpacked data
    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0], "01.jpg");

    // Verify nested sl structure
    if let serde_json::Value::String(e) = &result.sl.e {
        assert_eq!(e, "123");
    } else {
        panic!("sl.e should be a string");
    }
    assert_eq!(result.sl.m, "abc");
}

#[test]
fn test_unpack_packed_invalid_base() {
    let frame = "{}";
    let a = 100; // Base exceeds alphabet size (62)
    let c = 1;   // Must be > 0 to trigger the loop that calls encode()
    let data = vec!["dummy".to_string()];

    let result = unpack_packed(frame, a, c, data);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("exceeds supported alphabet size"), "Error message was: {}", err_msg);
}

#[test]
fn test_prompt_for_chapters_valid() {
    let mut input = std::io::Cursor::new("1-3,5\n");
    let chapters_count = 10;
    let result: Vec<usize> = prompt_for_chapters(&mut input, chapters_count).unwrap().collect();

    // 1-3 -> 0, 1, 2
    // 5 -> 4
    assert_eq!(result, vec![0, 1, 2, 4]);
}

#[test]
fn test_prompt_for_chapters_retry_on_invalid() {
    // First input is out of bounds (11 > 10), second is invalid format, third is valid.
    let mut input = std::io::Cursor::new("11\ninvalid\n2,4\n");
    let chapters_count = 10;
    let result: Vec<usize> = prompt_for_chapters(&mut input, chapters_count).unwrap().collect();

    assert_eq!(result, vec![1, 3]);
}

#[test]
fn test_prompt_for_chapters_dedup_and_sort() {
    let mut input = std::io::Cursor::new("5,3-4,3\n");
    let chapters_count = 10;
    let result: Vec<usize> = prompt_for_chapters(&mut input, chapters_count).unwrap().collect();

    // 5 -> 4
    // 3-4 -> 2, 3
    // 3 -> 2
    // Result should be sorted and unique: 2, 3, 4
    assert_eq!(result, vec![2, 3, 4]);
}
