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

    // Test full chapter URL (extra path segments after the ID are fine)
    assert_eq!(parse_id("https://tw.manhuagui.com/comic/12345/67890.html"), Some(12345));

    // Test site-relative paths as found in search result hrefs
    assert_eq!(parse_id("/comic/54544/"), Some(54544));
    assert_eq!(parse_id("/comic/54544"), Some(54544));
    assert_eq!(parse_id("/other/54544/"), None);

    // Test invalid inputs
    assert_eq!(parse_id("https://google.com/comic/12345"), None);
    assert_eq!(parse_id("abcde"), None);
    assert_eq!(parse_id(""), None);

    // The ID must end at a word boundary: trailing garbage glued to the
    // digits must not be silently accepted as a valid ID.
    assert_eq!(parse_id("123abc"), None);
    assert_eq!(parse_id("https://tw.manhuagui.com/comic/12345garbage"), None);
}

#[test]
fn test_unpack_packed() {
    // A simplified example of "packed" JavaScript code and its dictionary.
    // No space between '(' and '{': the RE_JSON regex \((\{.*?\})\) requires
    // the brace to immediately follow the parenthesis.
    let frame = "SMH.imgData({\"0\":{\"1\":\"123\",\"2\":\"abc\"},\"3\":\"/comic/\",\"4\":[\"01.jpg\"]})";
    let a = 10;
    let c = 5;
    let data = vec![
        "sl",    // 0
        "e",     // 1
        "m",     // 2
        "path",  // 3
        "files", // 4
    ];

    let result = unpack_packed(frame, a, c, &data).unwrap();

    // Verify the unpacked data
    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0], "01.jpg");

    // Verify nested sl structure
    match &result.sl.e {
        NumOrStr::Str(e) => assert_eq!(e, "123"),
        other => panic!("sl.e should be a string, got {:?}", other),
    }
    assert_eq!(result.sl.m, "abc");
}

#[test]
fn test_unpack_packed_invalid_base() {
    let frame = "{}";
    let c = 1;
    let data = vec!["dummy"];

    // Base exceeds alphabet size (62)
    let result = unpack_packed(frame, 100, c, &data);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("out of supported range"), "Error message was: {}", err_msg);

    // Exact boundary: 62 is the largest supported base, 63 must be rejected
    let result = unpack_packed(frame, 63, c, &data);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("out of supported range"));
}

#[test]
fn test_unpack_packed_base_too_small() {
    let frame = "{}";
    let c = 1;
    let data = vec!["dummy"];

    // Base 0 would panic via divide-by-zero without the up-front guard.
    let result = unpack_packed(frame, 0, c, &data);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("out of supported range"));

    // Base 1 would hang forever (value /= 1 never terminates) without the guard.
    let result = unpack_packed(frame, 1, c, &data);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("out of supported range"));
}

#[test]
fn test_unpack_packed_dictionary_size_mismatch() {
    // Test when c > data.len() - dictionary size doesn't match
    let frame = "{}";
    let a = 10;
    let c = 10;  // Request 10 items in dictionary
    let data = vec!["item1", "item2"];  // But only provide 2

    let result = unpack_packed(frame, a, c, &data);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("mismatch"), "Error message should mention mismatch: {}", err_msg);
}

#[test]
fn test_unpack_packed_empty_files_is_error() {
    // Same shape as test_unpack_packed, but the files array is empty: this
    // must be an error, not an empty chapter that would compress into an
    // empty .cbz and be treated as already downloaded forever.
    let frame = "SMH.imgData({\"0\":{\"1\":\"123\",\"2\":\"abc\"},\"3\":\"/comic/\",\"4\":[]})";
    let data = vec!["sl", "e", "m", "path", "files"];

    let result = unpack_packed(frame, 10, 5, &data);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("no image files"),
        "Error message was: {}",
        err_msg
    );
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
    // After grouped extraction and sorting, the order is:單話(第01話...) -> 單行本(...) -> 番外篇(...)
    // The first chapter should be "第01話"
    assert_eq!(chapters[0].name, "第01話");
    assert!(chapters[0].href.contains("/comic/40811/"));

    for (i, chapter) in chapters.iter().enumerate() {
        assert!(!chapter.name.is_empty(), "Chapter {} name should not be empty", i);
        assert!(!chapter.href.is_empty(), "Chapter {} href should not be empty", i);
        assert!(!chapter.group.is_empty(), "Chapter {} group should not be empty", i);
        assert!(chapter.href.starts_with("/comic/40811/"), "Chapter {} href should be valid path", i);
    }
}

#[test]
fn test_comic_metadata_extraction_adult_gated() {
    let html = load_test_html("comic_10528.html");
    let (title, chapters) = Comic::parse_comic_html(&html)
        .expect("Failed to parse adult-gated comic HTML");

    assert_eq!(title, "GATE奇幻自衛隊");
    assert!(!chapters.is_empty(), "Should find chapters via __VIEWSTATE fallback");
    for chapter in &chapters {
        assert!(!chapter.name.is_empty());
        assert!(!chapter.group.is_empty());
        assert!(chapter.href.starts_with("/comic/10528/"));
    }

    // Verify that at least one chapter has a real group (not the generic "Chapters" fallback)
    assert!(
        chapters.iter().any(|c| c.group != "Chapters"),
        "At least one chapter group should have a real tag extracted from h4, not the fallback"
    );
}

#[test]
fn test_comic_metadata_extraction_no_chapters_is_error() {
    // A page with a title but no chapter list (layout change, gated content
    // without __VIEWSTATE, error page) must fail instead of returning an
    // empty list that would trap the user in the chapter prompt loop.
    let html = r#"
        <html><body>
            <div class="book-title"><h1>某漫畫</h1></div>
            <p>本作品暫不提供觀看</p>
        </body></html>
    "#;
    assert!(Comic::parse_comic_html(html).is_err());
}

#[test]
fn test_comic_title_is_trimmed_and_must_not_be_blank() {
    // The h1 is often indented in the page source; untrimmed whitespace would
    // leak into the book directory name (and a trailing space makes the name
    // invalid on Windows). A whitespace-only title is treated as a missing one.
    let html = r#"
        <html><body>
            <div class="book-title"><h1>
                某漫畫
            </h1></div>
            <div class="chapter-list"><ul>
                <li><a href="/comic/1/101.html" title="第01話">第01話</a></li>
            </ul></div>
        </body></html>
    "#;
    let (title, _) = Comic::parse_comic_html(html).expect("Failed to parse comic HTML");
    assert_eq!(title, "某漫畫");

    let blank = html.replace("某漫畫", " ");
    assert!(Comic::parse_comic_html(&blank).is_err());
}

#[test]
fn test_extract_chapters_group_from_nearest_h4() {
    // The group must come from the nearest preceding sibling h4, skipping
    // unrelated elements (pager, tip blocks) sitting between the h4 and its
    // chapter-list, and ignoring h4s that belong to an earlier section.
    let html = r#"
        <h4><span>單話</span></h4>
        <div class="chapter-page"><a href="javascript:;">1-10</a></div>
        <div class="chapter-list"><ul>
            <li><a href="/comic/1/102.html" title="第02話">第02話</a></li>
            <li><a href="/comic/1/101.html" title="第01話">第01話</a></li>
        </ul></div>
        <h4><span>單行本</span></h4>
        <div class="chapter-list"><ul>
            <li><a href="/comic/1/201.html" title="第01卷">第01卷</a></li>
        </ul></div>
    "#;
    let document = Html::parse_fragment(html);
    let chapters = extract_chapters_with_groups(&document);
    let got: Vec<(&str, &str)> = chapters
        .iter()
        .map(|c| (c.group.as_str(), c.name.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("單話", "第01話"),
            ("單話", "第02話"),
            ("單行本", "第01卷"),
        ]
    );
}

#[test]
fn test_extract_chapters_group_fallback_without_h4() {
    // A chapter-list with no preceding h4 gets the generic fallback group.
    let html = r#"
        <div class="chapter-list"><ul>
            <li><a href="/comic/1/101.html" title="第01話">第01話</a></li>
        </ul></div>
    "#;
    let document = Html::parse_fragment(html);
    let chapters = extract_chapters_with_groups(&document);
    assert_eq!(chapters.len(), 1);
    assert_eq!(chapters[0].group, "Chapters");
}

#[test]
fn test_extract_chapters_skips_non_chapter_anchors() {
    // Anchors without both href and title are not chapters. They must be
    // ignored even when they sit inside the chapter-list's own <ul> — a single
    // pager entry or ad link there used to abort the whole book's parse.
    let html = r#"
        <h4><span>單話</span></h4>
        <div class="chapter-list"><ul>
            <li><a id="v1" href="javascript:;">1-10</a></li>
            <li><a href="/comic/1/102.html" title="第02話">第02話</a></li>
            <li><a title="no href">dead</a></li>
            <li><a href="/comic/1/101.html" title="第01話">第01話</a></li>
        </ul></div>
    "#;
    let document = Html::parse_fragment(html);
    let chapters = extract_chapters_with_groups(&document);
    let got: Vec<(&str, &str)> = chapters
        .iter()
        .map(|c| (c.name.as_str(), c.href.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("第01話", "/comic/1/101.html"),
            ("第02話", "/comic/1/102.html"),
        ],
        "only well-formed chapter anchors survive, still in reading order"
    );
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
fn test_prompt_for_chapters_retry_on_invalid() {
    // The prompt loop re-prompts on any input rejected by
    // parse_chapter_selection (which inputs are rejected is covered by
    // test_parse_chapter_selection) and passes the first accepted result
    // through unchanged.
    // First input is out of bounds (11 > 10), second is invalid format, third is valid.
    let mut input = std::io::Cursor::new("11\ninvalid\n1-3,5\n");
    let chapters_count = 10;
    let result: Vec<usize> = prompt_for_chapters(&mut input, chapters_count).unwrap();

    assert_eq!(result, vec![0, 1, 2, 4]);
}

#[test]
fn test_parse_chapter_selection() {
    // Valid inputs
    assert_eq!(parse_chapter_selection("1-3,5", 10), Some(vec![0, 1, 2, 4]));
    assert_eq!(parse_chapter_selection("10", 10), Some(vec![9]));
    assert_eq!(parse_chapter_selection(" 2 , 4 - 5 ", 10), Some(vec![1, 3, 4]));
    assert_eq!(parse_chapter_selection("1-5,3-7", 10), Some(vec![0, 1, 2, 3, 4, 5, 6]));
    assert_eq!(parse_chapter_selection("5,3-4,3", 10), Some(vec![2, 3, 4]));

    // Whole input rejected on any bad part
    assert_eq!(parse_chapter_selection("", 10), None);
    assert_eq!(parse_chapter_selection("0,5", 10), None);
    assert_eq!(parse_chapter_selection("11", 10), None);
    assert_eq!(parse_chapter_selection("1-11", 10), None);
    // A typo like "1-999999999" must be rejected by the bounds check before
    // the range is expanded, not allocate billions of entries first.
    assert_eq!(parse_chapter_selection("1-999999999", 10), None);
    assert_eq!(parse_chapter_selection("5-3", 10), None);
    assert_eq!(parse_chapter_selection("abc", 10), None);
    assert_eq!(parse_chapter_selection("1-2-3", 10), None);
    assert_eq!(parse_chapter_selection("-1", 10), None);
    assert_eq!(parse_chapter_selection("1,", 10), None);
}

#[test]
fn test_prompt_for_chapters_eof() {
    // stdin closed immediately: must error out instead of looping forever
    let mut input = std::io::Cursor::new("");
    assert!(prompt_for_chapters(&mut input, 10).is_err());
}

#[test]
fn test_prompt_for_chapters_eof_after_invalid_input() {
    // Invalid input followed by EOF: must error out after the retry
    let mut input = std::io::Cursor::new("999\n");
    assert!(prompt_for_chapters(&mut input, 10).is_err());
}

#[test]
fn test_prompt_for_comic_selection_eof() {
    let mut input = std::io::Cursor::new("");
    assert!(prompt_for_comic_selection(&mut input, 5).is_err());
}

#[test]
fn test_re_word() {
    let re = &*RE_WORD;

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
    let re = &*RE_JSON;

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
    let re = &*RE_CHAPTER_DATA;

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
    let re = &*RE_ILLEGAL_CHARS;

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
fn test_compress_chapter_atomic_and_excludes_part_files() {
    use std::fs;
    use std::io::Read;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter_test");
    fs::create_dir_all(&chapter_dir).unwrap();

    fs::write(chapter_dir.join("01_page.jpg"), "fake image data").unwrap();
    // A stale temp file from an interrupted download must not end up in the cbz
    fs::write(chapter_dir.join("02_page.jpg.part"), "partial data").unwrap();

    let zip_path = test_dir.join("chapter_test.cbz");

    Comic::compress_chapter(&chapter_dir, &["01_page.jpg".to_string()], &zip_path).unwrap();

    // The intermediate zip temp file must be renamed away
    assert!(zip_path.exists());
    assert!(!test_dir.join("chapter_test.cbz.part").exists());

    let mut archive = zip::ZipArchive::new(fs::File::open(&zip_path).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    assert_eq!(names, vec!["01_page.jpg"]);

    let mut content = String::new();
    archive
        .by_name("01_page.jpg")
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();
    assert_eq!(content, "fake image data");
}

#[test]
fn test_illegal_chars_unicode_handling() {
    // Test that Unicode characters are preserved (not replaced)
    let re = &*RE_ILLEGAL_CHARS;

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
fn test_illegal_chars_windows_forbidden() {
    // The regex covers every character Windows forbids in file names:
    // \, /, :, *, ?, ", <, >, | plus the C0 control characters and DEL.
    let re = &*RE_ILLEGAL_CHARS;
    let forbidden_by_regex = [
        '\\', '/', ':', '*', '?', '"', '<', '>', '|', '\0', '\n', '\r', '\t', '\x1f', '\x7f',
    ];

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
    let re = &*RE_ILLEGAL_CHARS;

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
fn test_compress_chapter_file_ordering() {
    // Page order comes from the caller's list, never from the directory.
    use tempfile::TempDir;
    use std::fs::File;

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();

    let pages: Vec<String> = ["01_page.jpg", "02_page.jpg", "03_page.jpg", "10_page.jpg", "20_page.jpg"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for filename in &pages {
        std::fs::write(chapter_dir.join(filename), b"image data").unwrap();
    }

    let zip_path = test_dir.join("test.cbz");

    // Call the actual compress_chapter method
    Comic::compress_chapter(&chapter_dir, &pages, &zip_path).unwrap();

    // Verify zip file was created and directory removed
    assert!(zip_path.exists());
    assert!(!chapter_dir.exists());

    // Verify the order of files inside the zip
    let file = File::open(&zip_path).unwrap();
    let mut archive = zip::read::ZipArchive::new(file).unwrap();

    assert_eq!(archive.len(), 5);
    for i in 0..archive.len() {
        let zip_file = archive.by_index(i).unwrap();
        assert_eq!(zip_file.name(), pages[i], "File at index {} should be {}", i, pages[i]);
    }
}

#[test]
fn test_compress_chapter_ignores_stale_files_from_previous_run() {
    // A chapter that gained pages between runs is re-downloaded under a wider
    // zero padding, leaving the old narrower names behind. Packing the
    // directory listing would both duplicate those pages and misplace them,
    // because '0' sorts before '_': "0_page.jpg" lands after "09_page.jpg".
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();

    // Stale single-digit names from the interrupted 9-page run.
    for i in 0..9 {
        std::fs::write(chapter_dir.join(format!("{}_page.jpg", i)), b"stale").unwrap();
    }
    // The current 12-page run.
    let pages: Vec<String> = (0..12).map(|i| format!("{:02}_page.jpg", i)).collect();
    for filename in &pages {
        std::fs::write(chapter_dir.join(filename), b"fresh").unwrap();
    }

    let zip_path = test_dir.join("test.cbz");
    Comic::compress_chapter(&chapter_dir, &pages, &zip_path).unwrap();

    let mut archive =
        zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    assert_eq!(names, pages, "stale pages must not leak into the archive");
}

#[test]
fn test_compress_chapter_missing_page_is_error() {
    // A page named in the list but absent from disk must fail loudly: silently
    // dropping it would publish a .cbz with a hole that is then cached forever.
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();
    std::fs::write(chapter_dir.join("0_page.jpg"), b"image data").unwrap();

    let pages = vec!["0_page.jpg".to_string(), "1_page.jpg".to_string()];
    let zip_path = test_dir.join("test.cbz");

    assert!(Comic::compress_chapter(&chapter_dir, &pages, &zip_path).is_err());
    // The chapter must stay unfinished so a later run can retry it.
    assert!(!zip_path.exists());
    assert!(chapter_dir.exists());
}

#[test]
fn test_directory_traversal_prevention() {
    // Sanitization must strip every path separator so a hostile title or
    // chapter name cannot escape the output directory when joined into a
    // path. "%2f" stays as-is: it is only meaningful to URL decoders and is
    // harmless literal text in a file name. A name made only of dots must not
    // survive as "." or ".." either, since joining those walks up a level.
    let cases = vec![
        ("../../../etc/passwd", ".._.._.._etc_passwd"),
        ("..\\..\\windows\\system32", ".._.._windows_system32"),
        ("..%2f..%2fetc", "..%2f..%2fetc"),
        ("/etc/passwd", "_etc_passwd"),
        ("\\windows\\system32", "_windows_system32"),
        ("..", "_"),
        (".", "_"),
    ];

    for (path, expected) in cases {
        let safe_path = sanitize(path);
        assert_eq!(safe_path, expected, "Sanitizing: {}", path);
        assert!(!safe_path.contains('/'), "Path: {} still contains /", path);
        assert!(!safe_path.contains('\\'), "Path: {} still contains \\", path);
        assert_ne!(Path::new(&safe_path).components().count(), 0);
        assert!(
            Path::new(&safe_path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "Path: {} produced a non-normal component",
            path
        );
    }
}

#[test]
fn test_sanitize_trims_and_never_returns_empty() {
    // Windows rejects names ending in a dot or a space, and an empty component
    // would make book_dir.join(..) point at the parent directory itself —
    // which compress_chapter would then delete.
    assert_eq!(sanitize("  第01話  "), "第01話");
    assert_eq!(sanitize("第01話."), "第01話");
    assert_eq!(sanitize("第01話 . . "), "第01話");
    assert_eq!(sanitize(""), "_");
    assert_eq!(sanitize("   "), "_");
    assert_eq!(sanitize("..."), "_");
    // Leading dots are legal in a file name and must be preserved.
    assert_eq!(sanitize("..foo"), "..foo");
    // Control characters become underscores rather than corrupting the name.
    assert_eq!(sanitize("第01話\n\t(完)"), "第01話__(完)");
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
        book_safe: "Test Comic".to_string(),
        book_dir: temp_dir.path().to_path_buf(),
    };

    let chap = ChapterStruct {
        sl: Sl { e: NumOrStr::Str("test_e".to_string()), m: "test_m".to_string() },
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
        book_safe: "Test Comic".to_string(),
        book_dir: temp_dir.path().to_path_buf(),
    };

    let chap = ChapterStruct {
        sl: Sl { e: NumOrStr::Str("test_e".to_string()), m: "test_m".to_string() },
        path: "/".to_string(),
        files: vec!["test.jpg".to_string()],
    };

    // If the logic is correct, it will see 0_test.jpg exists and skip network calls.
    // If it attempts to download, it will fail because the tunnel host is invalid.
    let result = comic.download_images(&chap, &chapter_dir, &bar, "http://localhost/chapter");

    assert!(result.is_ok(), "Should skip existing file and return Ok, but got error");
    // Skipped pages still have to be reported, or compress_chapter would omit them.
    assert_eq!(result.unwrap(), vec!["0_test.jpg".to_string()]);

    // Verify content remains unchanged and progress bar incremented
    let content = fs::read_to_string(&existing_file).unwrap();
    assert_eq!(content, original_content);
    assert_eq!(bar.position(), 1);
}

#[test]
fn test_sl_e_number_or_string_only() {
    // Valid: number
    let chap: ChapterStruct =
        serde_json::from_str(r#"{"sl":{"e":123,"m":"x"},"path":"/","files":[]}"#).unwrap();
    assert_eq!(chap.sl.e.to_string(), "123");

    // Valid: string
    let chap: ChapterStruct =
        serde_json::from_str(r#"{"sl":{"e":"abc","m":"x"},"path":"/","files":[]}"#).unwrap();
    assert_eq!(chap.sl.e.to_string(), "abc");

    // Invalid types must be rejected at deserialization time
    for bad in [
        r#"{"sl":{"e":true,"m":"x"},"path":"/","files":[]}"#,
        r#"{"sl":{"e":null,"m":"x"},"path":"/","files":[]}"#,
        r#"{"sl":{"e":[1],"m":"x"},"path":"/","files":[]}"#,
    ] {
        assert!(
            serde_json::from_str::<ChapterStruct>(bad).is_err(),
            "should reject: {}",
            bad
        );
    }
}

#[test]
fn test_parse_search_results_page() {
    let html = load_test_html("金田一.html");
    let (results, next_page) = parse_search_results(&html).unwrap();

    assert_eq!(results.len(), 10);
    assert_eq!(results[0].title, "金田一爸爸事件簿");
    assert_eq!(results[0].comic_id, 54544);
    assert_eq!(results[9].title, "金田一少年事件簿 鍊金術殺人事件");
    assert_eq!(results[9].comic_id, 4825);

    assert!(next_page.is_some());
    let next_url = next_page.unwrap();
    assert!(next_url.contains("_p2"));
}

#[test]
fn test_parse_search_results_last_page() {
    let html = load_test_html("金田一_p3.html");
    let (results, next_page) = parse_search_results(&html).unwrap();

    assert!(!results.is_empty());
    assert_eq!(results.len(), 8); // 28 total - 10 (page 1) - 10 (page 2) = 8
    assert!(next_page.is_none());
}

#[test]
fn test_multiple_ul_chapter_ordering() {
    // comic_1128.html spreads each section over several pager <ul>s, ordered
    // oldest block first, with the entries inside each <ul> newest first:
    //   單行本  ul[0] = 第22卷 … 第01卷, ul[1] = 第112卷 … 第23卷
    //   單話    ul[0..6], 58 + 90 * 5 = 508 entries
    // Reversing per <ul> while keeping the <ul> order must therefore produce
    // one continuous ascending run across the pager boundaries.
    let html = load_test_html("comic_1128.html");
    let (title, chapters) = Comic::parse_comic_html(&html).expect("Failed to parse comic HTML");

    assert_eq!(title, "ONE PIECE航海王");

    let names_in = |group: &str| -> Vec<String> {
        chapters
            .iter()
            .filter(|c| c.group == group)
            .map(|c| c.name.clone())
            .collect()
    };

    // 單行本 is fully sequential, so every position can be checked: 01 → 112.
    let names = names_in("單行本");
    assert_eq!(names.len(), 112, "單行本 should span both pager <ul>s");
    for i in 0..names.len() {
        let expected = format!("第{:02}卷", i + 1);
        assert_eq!(names[i], expected, "Position {} should be {}, got {}", i, expected, names[i]);
    }

    // 單話 names are irregular, so check the six-<ul> section by its ends and
    // by the entries straddling the first pager boundary (index 57 is the last
    // of ul[0], index 58 the first of ul[1]).
    let names = names_in("單話");
    assert_eq!(names.len(), 508, "單話 should span all six pager <ul>s");
    assert_eq!(names[0], "第00話前傳");
    assert_eq!(names[57], "第735回 藤虎的打算");
    assert_eq!(names[58], "第736回 最高干部迪亞曼蒂");
    assert_eq!(names[507], "第1185話");

    // The pager itself is a <ul> of "1-22"-style links inside a sibling
    // div.chapter-page, not inside .chapter-list; it must never be collected.
    assert!(
        chapters.iter().all(|c| c.href != "javascript:;"),
        "pager links leaked into the chapter list"
    );
}
