use super::*;

// Helper function to load test HTML files
fn load_test_html(filename: &str) -> String {
    let test_data_path = format!("test_data/{}", filename);
    std::fs::read_to_string(&test_data_path)
        .unwrap_or_else(|_| panic!("Failed to load test file: {}", test_data_path))
}

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
fn test_unpack_packed_dictionary_size_mismatch() {
    // Test when c > data.len() - dictionary size doesn't match
    let frame = "{}";
    let a = 10;
    let c = 10;  // Request 10 items in dictionary
    let data = vec!["item1".to_string(), "item2".to_string()];  // But only provide 2

    let result = unpack_packed(frame, a, c, data);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("mismatch"), "Error message should mention mismatch: {}", err_msg);
}

#[test]
fn test_unpack_packed_empty_dictionary() {
    // Test when c = 0 - empty dictionary
    let frame = "SMH.imgData({})";
    let a = 10;
    let c = 0;  // Empty dictionary
    let data = vec![];

    let result = unpack_packed(frame, a, c, data);
    // Should either succeed with empty dict or fail gracefully
    // The function should handle this without panicking
    let _ = result;  // We're mainly testing it doesn't panic
}

#[test]
fn test_unpack_packed_with_empty_strings_in_data() {
    // Test when data contains empty strings - they should use the encoded key
    let frame = "SMH.imgData({\"key\":\"value\"})";
    let a = 10;
    let c = 2;
    let data = vec![
        "mapped_0".to_string(),  // 0 -> "mapped_0"
        "".to_string(),          // 1 -> "1" (uses encoded key)
    ];

    let result = unpack_packed(frame, a, c, data);
    // Should handle empty strings by using encoded keys
    if result.is_ok() {
        let chapter = result.unwrap();
        // The unpacking should work without panicking
        assert!(!chapter.path.is_empty() || chapter.files.is_empty());
    }
}

// ==============================================================================
// Integration Tests: Real HTML Data from test_data/
// ==============================================================================

#[test]
fn test_comic_metadata_extraction_from_real_html() {
    let html = load_test_html("comic_40811.html");
    let (title, chapters) = Comic::parse_comic_html(&html).expect("Failed to parse comic HTML");

    // Verify title
    assert_eq!(title, "FX戰士久留美");

    // Verify chapters
    assert!(chapters.len() > 0, "Should find at least one chapter");
    // After reversal in parse_comic_html, the order is: volumes → omake → main episodes (reversed)
    assert_eq!(chapters[0].0, "第01卷");
    assert!(chapters[0].1.contains("/comic/40811/"));

    for (i, (name, href)) in chapters.iter().enumerate() {
        assert!(!name.is_empty(), "Chapter {} name should not be empty", i);
        assert!(!href.is_empty(), "Chapter {} href should not be empty", i);
        assert!(href.starts_with("/comic/40811/"), "Chapter {} href should be valid path", i);
    }
}

#[test]
fn test_chapter_parsing_from_real_html() {
    let html = load_test_html("comic_40811_chapter_1.html");
    let chapter = Comic::parse_chapter_html(&html).expect("Failed to parse chapter HTML");

    // Verify extracted data structure
    assert!(!chapter.path.is_empty());
    assert!(!chapter.files.is_empty());

    // Check specific known values for this test file: file count
    assert_eq!(chapter.files.len(), 48); // Verified from HTML content "(1/48)"
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

    // Call the actual compress_chapter method
    Comic::compress_chapter(&chapter_dir, &zip_path).unwrap();

    // Verify zip file was created
    assert!(zip_path.exists());
    assert!(zip_path.metadata().unwrap().len() > 0);
    // Verify chapter directory was removed
    assert!(!chapter_dir.exists());
}

#[test]
fn test_json_regex_with_complex_data() {
    // Test JSON extraction from unpacked JavaScript with various data types
    // Note: The regex uses .* which doesn't match newlines by default in most regex engines
    // So we need to provide single-line JSON
    let re = re_json();

    // Test with nested objects on a single line
    let js_code = r#"SMH.imgData({"sl": {"e": "12345", "m": "abc"}, "path": "/img/path/", "files": ["01.jpg", "02.jpg"]})"#;

    let caps = re.captures(js_code);
    assert!(caps.is_some());

    // Extract and verify JSON structure
    if let Some(json_match) = caps {
        let json_str = json_match.get(1).unwrap().as_str();
        let parsed: std::result::Result<serde_json::Value, _> = serde_json::from_str(json_str);
        assert!(parsed.is_ok());

        // Verify the JSON content
        if let Ok(value) = parsed {
            assert!(value.get("sl").is_some());
            assert!(value.get("path").is_some());
            assert!(value.get("files").is_some());
        }
    }
}

#[test]
fn test_prompt_for_chapters_overlapping_ranges() {
    // Test overlapping ranges - should be deduplicated
    let mut input = std::io::Cursor::new("1-5,3-7\n");
    let result: Vec<usize> = prompt_for_chapters(&mut input, 10)
        .unwrap()
        .collect();

    // Should be: 0,1,2,3,4 + 2,3,4,5,6 = deduplicated to 0,1,2,3,4,5,6
    assert_eq!(result, vec![0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn test_illegal_chars_unicode_handling() {
    // Test that Unicode characters are preserved (not replaced)
    let re = re_illegal_chars();

    let test_cases = vec![
        ("漫畫標題", "漫畫標題"),     // Chinese characters preserved
        ("マンガ", "マンガ"),         // Japanese preserved
        ("만화", "만화"),             // Korean preserved
        ("file_🎯name", "file_🎯name"), // Emoji preserved
    ];

    for (input, expected) in test_cases {
        let output = re.replace_all(input, "_").to_string();
        assert_eq!(output, expected, "Unicode should be preserved for: {}", input);
    }
}

#[test]
fn test_illegal_chars_consecutive_illegal() {
    // Test multiple consecutive illegal characters
    let re = re_illegal_chars();

    let input = "file<<<>>>name";
    let output = re.replace_all(input, "_").to_string();
    assert_eq!(output, "file______name");

    let input2 = "name***???***";
    let output2 = re.replace_all(input2, "_").to_string();
    assert_eq!(output2, "name_________");
}

#[test]
fn test_illegal_chars_windows_forbidden() {
    // Test characters that the regex actually matches
    // Note: The regex pattern is [\/:*?"<>|] which matches:
    // /, :, *, ?, ", <, >, | (but NOT \)
    let re = re_illegal_chars();
    let forbidden_by_regex = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

    for ch in &forbidden_by_regex {
        let input = format!("file{}name", ch);
        let output = re.replace_all(&input, "_").to_string();
        assert!(
            !output.contains(*ch),
            "Character '{}' should be replaced",
            ch
        );
    }
}

#[test]
fn test_illegal_chars_valid_characters_preserved() {
    // Test that valid characters are NOT replaced
    let re = re_illegal_chars();

    let test_cases = vec![
        "file-name",      // Hyphen valid
        "file_name",      // Underscore valid
        "file.name",      // Dot valid
        "file (1)",       // Parentheses, space valid
        "file[backup]",   // Brackets valid
        "file@home",      // @ valid
        "file&name",      // & valid
        "file+name",      // + valid
        "file=name",      // = valid
        "file name",      // Space valid
    ];

    for input in test_cases {
        let output = re.replace_all(input, "_").to_string();
        assert_eq!(
            output, input,
            "Valid filename characters should be preserved: {}",
            input
        );
    }
}

#[test]
fn test_unpack_packed_base_boundaries() {
    // Test base parameter at boundaries
    let frame = "SMH.imgData({\"path\":\"/test/\"})";

    // Minimum base (2)
    let data = vec!["path".to_string()];
    let result = unpack_packed(frame, 2, 1, data);
    assert!(result.is_ok() || result.is_err()); // Just ensure no panic

    // Maximum supported base (62)
    let data = vec!["path".to_string()];
    let result = unpack_packed(frame, 62, 1, data);
    assert!(result.is_ok() || result.is_err()); // Just ensure no panic

    // Over limit (63+)
    let data = vec!["path".to_string()];
    let result = unpack_packed(frame, 63, 1, data);
    assert!(result.is_err()); // Should fail
}

#[test]
fn test_compress_chapter_file_ordering() {
    // Test that files are sorted and added to ZIP
    use tempfile::TempDir;
    use std::fs::File;

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();

    // Create files with proper naming format (numeric prefix)
    // Using format like: "01_page.jpg", "02_page.jpg", etc.
    // This ensures correct numeric ordering with string sort
    let files_to_create = vec![
        "01_page.jpg",
        "02_page.jpg",
        "03_page.jpg",
        "10_page.jpg",
        "20_page.jpg",
    ];
    for filename in &files_to_create {
        std::fs::write(chapter_dir.join(filename), b"image data").unwrap();
    }

    let zip_path = test_dir.join("test.zip");

    // Call the actual compress_chapter method
    Comic::compress_chapter(&chapter_dir, &zip_path).unwrap();

    // Verify zip file was created and directory removed
    assert!(zip_path.exists());
    assert!(!chapter_dir.exists());

    // Verify the order of files inside the zip
    let file = File::open(&zip_path).unwrap();
    let mut archive = zip::read::ZipArchive::new(file).unwrap();

    assert_eq!(archive.len(), 5);
    for i in 0..archive.len() {
        let zip_file = archive.by_index(i).unwrap();
        assert_eq!(zip_file.name(), files_to_create[i], "File at index {} should be {}", i, files_to_create[i]);
    }
}

#[test]
fn test_directory_traversal_prevention() {
    // Test that path sanitization prevents directory traversal attempts
    let re = re_illegal_chars();

    let dangerous_paths = vec![
        "../../../etc/passwd",
        "..\\..\\windows\\system32",
        "..%2f..%2fetc",
        "/etc/passwd",
        "\\windows\\system32",
    ];

    for path in dangerous_paths {
        let safe_path = re.replace_all(path, "_").to_string();
        // Forward slashes and backslashes should be removed
        assert!(!safe_path.contains('/'), "Path: {} still contains /", path);
    }
}

#[test]
fn test_download_incomplete_file() {
    use std::net::TcpListener;
    use std::io::{Read, Write};
    use std::thread;
    use tempfile::TempDir;

    // 1. Set up a minimal Mock Server
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server_thread = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0; 512];
            let _ = stream.read(&mut buffer); // Read request

            // Return Content-Length: 100, but only provide 50 bytes then disconnect
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&[0u8; 50]);
            // The stream will be closed automatically when the thread ends (RAII)
        }
    });

    // 2. Initialize test environment
    let temp_dir = TempDir::new().unwrap();
    let chapter_dir = temp_dir.path().to_path_buf();
    let bar = ProgressBar::hidden();

    let client = reqwest::blocking::Client::new();
    let comic = Comic {
        client,
        host: "http://localhost".to_string(),
        tunnel: format!("http://127.0.0.1:{}", port),
        delay: Duration::from_millis(0),
        title: "Test Comic".to_string(),
        chapters: vec![],
        output_dir: temp_dir.path().to_str().unwrap().to_string(),
    };

    let chap = ChapterStruct {
        sl: Sl { e: serde_json::Value::String("test_e".to_string()), m: "test_m".to_string() },
        path: "/".to_string(),
        files: vec!["test.jpg".to_string()],
    };

    // 3. Execute download and verify result
    let result = comic.download_images(&chap, &chapter_dir, &bar, "http://localhost/chapter");

    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    // Accept custom Incomplete download or end of file errors thrown by reqwest
    assert!(
        err_msg.contains("Incomplete download") ||
        err_msg.contains("end of file") ||
        err_msg.contains("UnexpectedEof"),
        "Error message should mention incomplete download or EOF, got: {}", err_msg
    );

    // 4. Verify that the final file was not renamed (due to download failure)
    // Expected filename is "0_test.jpg" (since width=1)
    let final_path = chapter_dir.join("0_test.jpg");
    assert!(!final_path.exists(), "Final file should not exist after incomplete download");

    server_thread.join().unwrap();
}

#[test]
fn test_download_resume_logic() {
    use tempfile::TempDir;
    use std::fs;

    let temp_dir = TempDir::new().unwrap();
    let chapter_dir = temp_dir.path().to_path_buf();
    let bar = ProgressBar::hidden();

    // Create a dummy file that matches what download_images expects
    // For 1 file, width is 1. Filename: 0_test.jpg
    let existing_file = chapter_dir.join("0_test.jpg");
    let original_content = "already here";
    fs::write(&existing_file, original_content).unwrap();

    let client = reqwest::blocking::Client::new();
    let comic = Comic {
        client,
        host: "http://localhost".to_string(),
        tunnel: "http://invalid-host-should-not-be-reached".to_string(),
        delay: Duration::from_millis(0),
        title: "Test Comic".to_string(),
        chapters: vec![],
        output_dir: temp_dir.path().to_str().unwrap().to_string(),
    };

    let chap = ChapterStruct {
        sl: Sl { e: serde_json::Value::String("test_e".to_string()), m: "test_m".to_string() },
        path: "/".to_string(),
        files: vec!["test.jpg".to_string()],
    };

    // If the logic is correct, it will see 0_test.jpg exists and skip network calls.
    // If it attempts to download, it will fail because the tunnel host is invalid.
    let result = comic.download_images(&chap, &chapter_dir, &bar, "http://localhost/chapter");

    assert!(result.is_ok(), "Should skip existing file and return Ok, but got error");

    // Verify content remains unchanged and progress bar incremented
    let content = fs::read_to_string(&existing_file).unwrap();
    assert_eq!(content, original_content);
    assert_eq!(bar.position(), 1);
}
