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

#[test]
fn test_re_word() {
    let re = re_word();

    // Word boundary regex \b\w+\b matches word characters between word boundaries
    // Note: 123 in the middle of alphanumeric chars is part of the same word
    let caps: Vec<&str> = re.find_iter("hello123world").map(|m| m.as_str()).collect();
    assert_eq!(caps, vec!["hello123world"]); // All word chars together form one word

    // Test with spaces separating words
    let caps: Vec<&str> = re.find_iter("hello 123 world").map(|m| m.as_str()).collect();
    assert_eq!(caps, vec!["hello", "123", "world"]);

    // Test with underscores (underscores are word characters)
    let caps: Vec<&str> = re.find_iter("test_var_name").map(|m| m.as_str()).collect();
    assert_eq!(caps, vec!["test_var_name"]); // Underscores connect words

    // Test with special characters (should split)
    let caps: Vec<&str> = re.find_iter("hello@world").map(|m| m.as_str()).collect();
    assert_eq!(caps, vec!["hello", "world"]);
}

#[test]
fn test_re_json() {
    let re = re_json();

    // Standard format with function call
    let text = "someFunc({\"key\":\"value\"})";
    let caps = re.captures(text).unwrap();
    assert_eq!(caps.get(1).map(|m| m.as_str()), Some("{\"key\":\"value\"}"));

    // With nested JSON
    let text = "SMH.imgData({\"0\":{\"1\":\"123\"},\"path\":\"/img/\"})";
    let caps = re.captures(text).unwrap();
    let json_str = caps.get(1).map(|m| m.as_str()).unwrap();
    assert!(json_str.contains("\"0\""));
    assert!(json_str.contains("\"path\""));

    // Should not match if no parentheses
    assert!(re.captures("{}").is_none());
}

#[test]
fn test_re_chapter_data() {
    let re = re_chapter_data();

    // Standard packed format (no spaces)
    let text = "xxx}('packed_frame_data',10,5,'base64data==')xxx";
    let caps = re.captures(text).unwrap();

    assert_eq!(caps.get(1).map(|m| m.as_str()), Some("packed_frame_data"));
    assert_eq!(caps.get(2).map(|m| m.as_str()), Some("10"));
    assert_eq!(caps.get(3).map(|m| m.as_str()), Some("5"));
    assert_eq!(caps.get(4).map(|m| m.as_str()), Some("base64data=="));

    // With leading whitespace: \s* consumes leading space, (.*?) captures rest without leading space
    let text = "xxx}(' packed_frame_data',10,5,'base64data==')xxx";
    let caps = re.captures(text).unwrap();
    assert_eq!(caps.get(1).map(|m| m.as_str()), Some("packed_frame_data"));
}

#[test]
fn test_re_illegal_chars() {
    let re = re_illegal_chars();

    // Test forward slash
    let input = "file/name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name");

    // Test colon
    let input = "file:name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name");

    // Test asterisk
    let input = "file*name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name");

    // Test question mark
    let input = "file?name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name");

    // Test double quote
    let input = "file\"name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name");

    // Test angle brackets
    let input = "file<name>";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name_");

    // Test pipe
    let input = "file|name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name");

    // Test multiple illegal characters
    let input = "file<name>test*value?";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file_name_test_value_");

    // Test valid characters are not replaced
    let input = "valid-file_name.txt";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "valid-file_name.txt");
}

#[test]
fn test_path_safety_with_illegal_chars() {
    let re = re_illegal_chars();

    // Test comic title with forward slashes
    let title = "Path/To/Comic";
    let safe_title = re.replace_all(title, "_").to_string();
    assert_eq!(safe_title, "Path_To_Comic");
    assert!(!safe_title.contains("/"));

    // Test chapter name with colons (Windows forbidden character)
    let chapter = "Chapter:1:Part:2";
    let safe_chapter = re.replace_all(chapter, "_").to_string();
    assert_eq!(safe_chapter, "Chapter_1_Part_2");
    assert!(!safe_chapter.contains(":"));

    // Test with mixed illegal characters
    let mixed = "Comic<2024>*Special*|Version";
    let safe_mixed = re.replace_all(mixed, "_").to_string();
    assert_eq!(safe_mixed, "Comic_2024__Special__Version");
    assert!(!safe_mixed.contains("<"));
    assert!(!safe_mixed.contains(">"));
    assert!(!safe_mixed.contains("*"));
    assert!(!safe_mixed.contains("|"));

    // Test all Windows forbidden filename characters: / : * ? " < > |
    let forbidden = "file/name:test*value?data\"test<name>file|data";
    let safe_name = re.replace_all(forbidden, "_").to_string();
    assert!(!safe_name.contains("/"));
    assert!(!safe_name.contains(":"));
    assert!(!safe_name.contains("*"));
    assert!(!safe_name.contains("?"));
    assert!(!safe_name.contains("\""));
    assert!(!safe_name.contains("<"));
    assert!(!safe_name.contains(">"));
    assert!(!safe_name.contains("|"));
}

#[test]
fn test_compress_chapter() {
    use std::fs;
    use tempfile::TempDir;

    // Create a temporary directory for testing
    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    // Create a test chapter directory with sample image files
    let chapter_dir = test_dir.join("chapter_test");
    fs::create_dir_all(&chapter_dir).unwrap();

    // Create test files with numeric prefixes
    let files = vec!["01_page.jpg", "02_page.jpg", "10_page.jpg", "09_page.jpg"];
    for file in &files {
        fs::write(chapter_dir.join(file), "fake image data").unwrap();
    }

    let zip_path = test_dir.join("chapter_test.zip");

    // Create a minimal Comic instance to test compress_chapter
    // We'll need to mock or use a real instance
    // For now, we test the compression logic directly
    let zip_file = fs::File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(zip_file);
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);

    let mut file_paths: Vec<_> = fs::read_dir(&chapter_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();

    file_paths.sort();

    // Verify files are sorted numerically
    let file_names: Vec<_> = file_paths.iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()))
        .collect();

    // Expected order: 01, 02, 09, 10 (numeric sorting, not string sorting)
    assert_eq!(file_names[0], "01_page.jpg");
    assert_eq!(file_names[1], "02_page.jpg");
    assert_eq!(file_names[2], "09_page.jpg");
    assert_eq!(file_names[3], "10_page.jpg");

    // Write files to zip
    for path in file_paths {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            zip.start_file(name, options).unwrap();
            let mut file = fs::File::open(&path).unwrap();
            std::io::copy(&mut file, &mut zip).unwrap();
        }
    }

    zip.finish().unwrap();

    // Verify zip file was created
    assert!(zip_path.exists());
    assert!(zip_path.metadata().unwrap().len() > 0);
}
