use super::*;
use std::net::{TcpListener, TcpStream};
use tempfile::TempDir;

// Helper function to load test HTML files
fn load_test_html(filename: &str) -> String {
    let test_data_path = format!("test_data/{}", filename);
    std::fs::read_to_string(&test_data_path)
        .unwrap_or_else(|_| panic!("Failed to load test file: {}", test_data_path))
}

/// A `Comic` with everything but the tunnel and the output directory stubbed
/// out. That is all the download-side methods read, and building one by hand at
/// every call site only invites the copies to drift apart.
fn test_comic(tunnel: &str, book_dir: &Path) -> Comic {
    Comic {
        client: reqwest::blocking::Client::new(),
        tunnel: Url::parse(tunnel).expect("test tunnel must be a valid absolute URL"),
        delay: Duration::from_millis(0),
        title: "Test Comic".to_string(),
        chapters: vec![],
        book_safe: "Test Comic".to_string(),
        book_dir: book_dir.to_path_buf(),
        landing: Url::parse("http://localhost/comic/")
            .expect("test landing page must be a valid absolute URL"),
    }
}

/// The chapter page a download sends back as its `referer`. None of the mock
/// servers below look at it, so one stand-in serves every call site — it is
/// here as a `Url` because that is what `download_images` takes.
fn test_referer() -> Url {
    Url::parse("http://localhost/chapter").expect("test referer must be a valid absolute URL")
}

/// A chapter served straight off the tunnel root. `files` stays a parameter
/// rather than being baked in because the page names the download tests assert
/// on are zero-padded to the width of the page count, so how many pages a
/// chapter has belongs at the call site.
fn test_chapter(files: &[&str]) -> ChapterStruct {
    ChapterStruct {
        sl: Sl {
            e: NumOrStr::Str("test_e".to_string()),
            m: "test_m".to_string(),
        },
        path: "/".to_string(),
        files: files.iter().map(|f| f.to_string()).collect(),
    }
}

/// What every `download_images` test starts from: a comic on `tunnel`, a hidden
/// progress bar to count pages with, and one scratch directory. That directory
/// is where the pages land, and it doubles as the comic's `book_dir`, which
/// `download_images` never reads.
struct DownloadFixture {
    comic: Comic,
    bar: ProgressBar,
    dir: TempDir,
}

impl DownloadFixture {
    fn new(tunnel: &str) -> Self {
        let dir = TempDir::new().unwrap();
        DownloadFixture {
            comic: test_comic(tunnel, dir.path()),
            bar: ProgressBar::hidden(),
            dir,
        }
    }

    /// Download a `test_chapter` of `files` into the scratch directory.
    fn download(&self, files: &[&str]) -> Result<Vec<String>> {
        self.comic.download_images(
            &test_chapter(files),
            self.dir.path(),
            &self.bar,
            &test_referer(),
        )
    }
}

/// The dictionary the base-10 unpack tests pack against: key `0` is "sl", `1`
/// "e", `2` "m", `3` "path" and `4` "files". A test that needs one word more
/// appends it, as key `5`.
const PACKED_DICT: [&str; 5] = ["sl", "e", "m", "path", "files"];

/// The `({…})` payload call of a frame packed against `PACKED_DICT`, with
/// `files` spliced in as the raw text of the files array. No space between `(`
/// and `{`: the payload is located by searching for the literal "({", so the
/// brace has to immediately follow the parenthesis.
fn packed_payload(files: &str) -> String {
    format!(r#"({{"0":{{"1":"123","2":"abc"}},"3":"/comic/","4":{files}}})"#)
}

/// Assert that a prompt gave up because its input closed, and not over some
/// other error on the way there.
fn assert_input_closed<T: std::fmt::Debug>(result: std::result::Result<T, AppError>) {
    let err = result.expect_err("a closed input must end the prompt");
    assert!(
        matches!(&err, AppError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
        "expected the input to be reported closed, got: {}",
        err
    );
}

/// Assert that `result` failed, with an error whose message mentions `needle`.
/// Either failure names what came back instead. The caller is tracked, so the
/// panic points at the test rather than at this helper.
#[track_caller]
fn assert_err_contains<T: std::fmt::Debug>(result: std::result::Result<T, AppError>, needle: &str) {
    match result {
        Ok(value) => panic!("expected an error mentioning {needle:?}, got Ok({value:?})"),
        Err(err) => assert!(
            err.to_string().contains(needle),
            "expected an error mentioning {needle:?}, got: {err}"
        ),
    }
}

/// A JPEG signature, for mock responses that have to get past
/// `looks_like_image`.
const JPEG_SOI: &[u8] = b"\xFF\xD8\xFF\xE0";

/// Serve `count` connections on a fresh local port, one request each, answering
/// every request with whatever `respond` builds from its request line. Returns
/// the base URL to hand `test_comic` as the tunnel, and the server thread, which
/// yields the request lines in the order they arrived.
fn serve(
    count: usize,
    respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        (0..count)
            .map(|_| {
                let (mut stream, _) = listener.accept().unwrap();
                let line = read_request_line(&mut stream);
                // The client may already have hung up — a rejected page is
                // dropped after its first bytes — so a failed write is no
                // failure of the server's.
                let _ = stream.write_all(&respond(&line));
                line
            })
            .collect::<Vec<_>>()
    });
    (base, server)
}

/// The request line of the request waiting on `stream`. The rest of the head is
/// read and dropped rather than left in the socket: closing a socket with bytes
/// still unread makes Linux answer with a reset, which the client can report in
/// place of the response the test is about.
fn read_request_line(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    head.lines().next().unwrap_or_default().to_string()
}

/// A 200 response carrying `body`. The connection is closed behind it, so the
/// client never tries to reuse it for the next page.
fn http_200(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[test]
fn test_parse_id() {
    // Test pure numeric ID
    assert_eq!(parse_id("12345"), Some(12345));

    // Test standard web URL
    assert_eq!(
        parse_id("https://www.manhuagui.com/comic/12345"),
        Some(12345)
    );
    assert_eq!(
        parse_id("http://www.manhuagui.com/comic/12345"),
        Some(12345)
    );

    // Test URL with trailing slash
    assert_eq!(
        parse_id("https://www.manhuagui.com/comic/12345/"),
        Some(12345)
    );

    // Test mobile or other subdomain URLs
    assert_eq!(parse_id("https://m.manhuagui.com/comic/12345"), Some(12345));
    assert_eq!(
        parse_id("https://tw.manhuagui.com/comic/12345"),
        Some(12345)
    );

    // Test full chapter URL (extra path segments after the ID are fine)
    assert_eq!(
        parse_id("https://tw.manhuagui.com/comic/12345/67890.html"),
        Some(12345)
    );

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
    assert_eq!(
        parse_id("https://tw.manhuagui.com/comic/12345garbage"),
        None
    );

    // Surrounding whitespace is stripped: a pasted URL or an href written with
    // padding must parse the same as the bare form.
    assert_eq!(parse_id("  12345  "), Some(12345));
    assert_eq!(parse_id("\t/comic/54544/\n"), Some(54544));
    assert_eq!(
        parse_id(" https://tw.manhuagui.com/comic/12345 "),
        Some(12345)
    );

    // Trimming must not turn garbage into a match.
    assert_eq!(parse_id("   "), None);
    assert_eq!(parse_id("  abcde  "), None);
    assert_eq!(parse_id("  123abc  "), None);

    // Non-ASCII glued to the digits is trailing garbage too, which is why the
    // `\b` is left Unicode-aware while the classes around it are not.
    assert_eq!(parse_id("12345漢化"), None);
    assert_eq!(parse_id("https://tw.manhuagui.com/comic/12345漢"), None);

    // Full-width digits are not digits: they never survived the parse into a
    // usize, and now they do not match in the first place.
    assert_eq!(parse_id("１２３４５"), None);
}

#[test]
fn test_unpack_packed() {
    // A simplified example of "packed" JavaScript code and its dictionary.
    let frame = format!("SMH.imgData{}", packed_payload(r#"["01.jpg"]"#));

    let result = unpack_packed(&frame, 10, 5, &PACKED_DICT).unwrap();

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
fn test_unpack_packed_base_out_of_range() {
    // Both ends of the guard. 1 and 63 are the first bases past each end; that
    // 2 and 62 themselves are accepted is what the base-2 and base-62 tests
    // below establish. Without the guard, 0 would divide by zero, 1 would never
    // finish dividing, and 63 and up could index past the end of DIGITS.
    //
    // The dictionary is empty so that none of that can happen even with the
    // guard gone: `encode` is never reached, and a missing or loosened guard
    // fails here as the wrong error message — not as a panic, and not as a
    // test that never returns.
    let frame = "{}";

    for base in [0, 1, 63, 100] {
        let err_msg = unpack_packed(frame, base, 0, &[])
            .expect_err("base out of range must be rejected")
            .to_string();
        assert!(
            err_msg.contains("out of supported range"),
            "Base {} error message was: {}",
            base,
            err_msg
        );
    }
}

#[test]
fn test_unpack_packed_keeps_the_text_before_the_first_word() {
    // The substitution walks the frame match by match and copies the gaps
    // between them itself. Every other frame in the suite opens on a word, so
    // the gap ahead of the first match is only ever empty there. Here the frame
    // opens on `({"` and the first word is the key after it, which puts the
    // whole `({` that locates the payload inside that leading gap: lose it and
    // there is nothing left to find.
    let frame = packed_payload(r#"["01.jpg"]"#);

    let result = unpack_packed(&frame, 10, 5, &PACKED_DICT).unwrap();

    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files, vec!["01.jpg".to_string()]);
    assert_eq!(result.sl.m, "abc");
}

#[test]
fn test_unpack_packed_substitutes_a_key_written_against_non_ascii_text() {
    // The packer's word class is JavaScript's `[0-9A-Za-z_]`, so a character it
    // does not count as a word — CJK here — is left in the frame verbatim, and
    // a key can end up with no separator between it and that text. In
    // "第5話.jpg" only the `5` is a key; a Unicode-aware `\w+` would swallow the
    // whole run as one word, find no entry for it, and leave the key in place.
    let frame = format!("SMH.imgData{}", packed_payload(r#"["第5話.jpg"]"#));
    // "01" is key 5.
    let data = [PACKED_DICT.as_slice(), &["01"][..]].concat();

    let result = unpack_packed(&frame, 10, 6, &data).unwrap();

    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files, vec!["第01話.jpg".to_string()]);
}

#[test]
fn test_unpack_packed_payload_with_brace_paren_in_a_string_value() {
    // Only word runs are substituted, so punctuation inside a title survives
    // the unpacking verbatim and lands in the JSON payload. A chapter named
    // "第01話(完})" therefore puts a `})` in the middle of the object, and
    // ending the payload at the first `})` would truncate it into invalid JSON.
    // The keys after the offending value are the ones that must still arrive.
    let frame = "SMH.imgData({\"0\":{\"1\":\"123\",\"2\":\"abc\"},\"5\":\"第01話(完})\",\"3\":\"/comic/\",\"4\":[\"01.jpg\"]}).preInit();";
    // "cname" is key 5.
    let data = [PACKED_DICT.as_slice(), &["cname"][..]].concat();

    let result = unpack_packed(frame, 10, 6, &data).expect("payload must survive an embedded `})`");

    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files, vec!["01.jpg".to_string()]);
}

#[test]
fn test_unpack_packed_without_json_is_error() {
    // The unpacked script must still contain a `({...})` payload; a page whose
    // frame carries something else fails with the dedicated message rather
    // than reaching serde.
    assert_err_contains(
        unpack_packed("SMH.imgData is not here", 10, 1, &["sl"]),
        "Could not find JSON data",
    );
}

#[test]
fn test_unpack_packed_dictionary_size_mismatch() {
    // Test when c > data.len() - dictionary size doesn't match
    let frame = "{}";
    let a = 10;
    let c = 10; // Request 10 items in dictionary
    let data = vec!["item1", "item2"]; // But only provide 2

    assert_err_contains(unpack_packed(frame, a, c, &data), "mismatch");
}

#[test]
fn test_unpack_packed_ignores_words_past_the_declared_size() {
    // The dictionary is held to the size the script declares, not to however
    // many words the payload happens to split into: a sixth word would be key
    // `5`, and must not reach the `5` in the file name.
    let frame = format!("SMH.imgData{}", packed_payload(r#"["5.jpg"]"#));
    let data = [PACKED_DICT.as_slice(), &["surplus"][..]].concat();

    let result = unpack_packed(&frame, 10, 5, &data).unwrap();

    assert_eq!(result.files, vec!["5.jpg".to_string()]);
}

#[test]
fn test_unpack_packed_base_2_writes_keys_most_significant_digit_first() {
    // Base 2 is the smallest the guard accepts, and the one where keys run past
    // a single digit within `PACKED_DICT` itself: word 2 is key `10`, word 4 is
    // `100`. Digits left least significant first would name them `01` and
    // `001` instead, and neither `m` nor `files` would arrive.
    let frame = r#"SMH.imgData({"0":{"1":"123","10":"abc"},"11":"/comic/","100":["01.jpg"]})"#;

    let result = unpack_packed(frame, 2, 5, &PACKED_DICT).unwrap();

    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files, vec!["01.jpg".to_string()]);
    assert_eq!(result.sl.m, "abc");
}

#[test]
fn test_unpack_packed_base_62_uses_the_packer_alphabet() {
    // Base 62 is the largest the guard accepts, and the one real chapter pages
    // are packed in. Its digits run 0-9, then a-z, then A-Z — a table with the
    // two letter cases the other way round swaps `sl` and `path` below — and
    // word 62 is the first to take two of them. The words the frame does not
    // use are left empty.
    let mut data = vec![""; 63];
    data[10] = "sl"; // a
    data[11] = "e"; // b
    data[36] = "path"; // A
    data[61] = "files"; // Z
    data[62] = "m"; // 10
    let frame = r#"SMH.imgData({"a":{"b":"123","10":"abc"},"A":"/comic/","Z":["01.jpg"]})"#;

    let result = unpack_packed(frame, 62, 63, &data).unwrap();

    assert_eq!(result.path, "/comic/");
    assert_eq!(result.files, vec!["01.jpg".to_string()]);
    assert_eq!(result.sl.m, "abc");
}

#[test]
fn test_unpack_packed_empty_files_is_error() {
    // Same shape as test_unpack_packed, but the files array is empty: this
    // must be an error, not an empty chapter that would compress into an
    // empty .cbz and be treated as already downloaded forever.
    let frame = format!("SMH.imgData{}", packed_payload("[]"));

    assert_err_contains(unpack_packed(&frame, 10, 5, &PACKED_DICT), "no image files");
}

#[test]
fn test_unescape_packed_frame() {
    // The common case: no escapes at all, and the input is handed back
    // without a copy.
    assert!(matches!(
        unescape_packed_frame("SMH.imgData({})"),
        Cow::Borrowed(_)
    ));

    // The two escapes the packer's escaper writes.
    assert_eq!(unescape_packed_frame(r"It\'s"), "It's");
    assert_eq!(unescape_packed_frame(r"a\\b"), r"a\b");
    // An escaped backslash directly ahead of an escaped quote: the scan must
    // pair the backslashes left to right, not pick the middle two.
    assert_eq!(unescape_packed_frame(r"\\\'"), r"\'");
    // A JSON escape travels with its backslash doubled and must come back
    // single, ready for serde to decode.
    assert_eq!(unescape_packed_frame(r#"\\"quoted\\""#), r#"\"quoted\""#);
    // Anything the packer never writes is kept verbatim, dangling tail
    // included, rather than guessed at.
    assert_eq!(unescape_packed_frame(r"a\nb"), r"a\nb");
    assert_eq!(unescape_packed_frame(r"tail\"), r"tail\");
}

// ==============================================================================
// Integration Tests: Real HTML Data from test_data/
// ==============================================================================

#[test]
fn test_comic_metadata_extraction_from_real_html() {
    let html = load_test_html("comic_40811.html");
    let (title, chapters) = parse_comic_html(&html).expect("Failed to parse comic HTML");

    // Verify title
    assert_eq!(title, "FX戰士久留美");

    // Verify chapters
    assert!(chapters.len() > 0, "Should find at least one chapter");
    // Nothing is sorted: sections keep their document order (單話 -> 單行本 ->
    // 番外篇 in this page) and only the entries within each <ul> are reversed
    // into reading order, so the very first chapter is 單話's oldest one.
    assert_eq!(chapters[0].name, "第01話");

    for (i, chapter) in chapters.iter().enumerate() {
        assert!(
            !chapter.name.is_empty(),
            "Chapter {} name should not be empty",
            i
        );
        assert!(
            !chapter.href.is_empty(),
            "Chapter {} href should not be empty",
            i
        );
        assert!(
            chapter.href.starts_with("/comic/40811/"),
            "Chapter {} href should be valid path",
            i
        );
    }
}

#[test]
fn test_comic_metadata_extraction_adult_gated() {
    let html = load_test_html("comic_10528.html");
    let (title, chapters) =
        parse_comic_html(&html).expect("Failed to parse adult-gated comic HTML");

    assert_eq!(title, "GATE奇幻自衛隊");
    assert!(
        !chapters.is_empty(),
        "Should find chapters via __VIEWSTATE fallback"
    );
    for chapter in &chapters {
        assert!(!chapter.name.is_empty());
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
    assert!(parse_comic_html(html).is_err());
}

#[test]
fn test_comic_metadata_extraction_undecodable_viewstate_is_no_chapters() {
    // A __VIEWSTATE that is not lz-string data at all — an unrelated ASP.NET
    // view state that merely uses the id — must not abort the parse with a
    // decoder complaint. The page still has no chapters, and that is the error
    // that describes it.
    let html = r#"
        <html><body>
            <div class="book-title"><h1>某漫畫</h1></div>
            <input type="hidden" id="__VIEWSTATE" value="!!!!">
        </body></html>
    "#;
    assert_err_contains(parse_comic_html(html), "No chapters found");
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
    let (title, _) = parse_comic_html(html).expect("Failed to parse comic HTML");
    assert_eq!(title, "某漫畫");

    let blank = html.replace("某漫畫", " ");
    assert!(parse_comic_html(&blank).is_err());
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
        vec![("單話", "第01話"), ("單話", "第02話"), ("單行本", "第01卷"),]
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
fn test_extract_chapters_trims_names() {
    // The parser hands an attribute back with its padding intact, and the name
    // is what the listing and the progress bar print, so it is trimmed the way
    // the book title is.
    let html = r#"
        <div class="chapter-list"><ul>
            <li><a href="/comic/1/101.html" title="&#9; 第01話&#10;">第01話</a></li>
        </ul></div>
    "#;
    let chapters = extract_chapters_with_groups(&Html::parse_fragment(html));
    assert_eq!(chapters.len(), 1);
    assert_eq!(chapters[0].name, "第01話");
}

#[test]
fn test_extract_chapters_skips_non_chapter_anchors() {
    // Two separate filters keep non-chapters out, and they cover different
    // cases:
    //   * The real pager (div.chapter-page, see comic_1128.html) is a <ul> of
    //     anchors that carry *both* href and title, so only the .chapter-list
    //     scoping excludes it — the selector alone would happily match it.
    //   * Anchors missing either attribute — an ad or a "more" link inside the
    //     list's own <ul> — are dropped by the selector instead of aborting the
    //     whole book's parse.
    let html = r#"
        <h4><span>單話</span></h4>
        <div class="chapter-page"><ul>
            <li><a href="javascript:;" title="1-10">1-10<i></i></a></li>
            <li class="on"><a href="javascript:;" title="11-20">11-20<i></i></a></li>
        </ul></div>
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
    let chapter = parse_chapter_html(&html).expect("Failed to parse chapter HTML");

    // Verify extracted data structure
    assert!(!chapter.path.is_empty());

    // Check specific known values for this test file: file count. 48 is the
    // length of the files array in this page's packed chapter data.
    assert_eq!(chapter.files.len(), 48);
}

#[test]
fn test_parse_chapter_html_unescapes_the_frame_literal() {
    // The frame sits in the page as a single-quoted JS string literal, so a
    // title carrying quotes arrives escaped: `'` as `\'`, the backslash of a
    // JSON `\"` as `\\`. eval strips these before a browser's unpacker runs;
    // parse_chapter_html has to do the same, or serde is handed `\'` — not a
    // JSON escape — and the chapter fails over fields ChapterStruct never
    // even deserializes.
    //
    // The base64 payload is borrowed from the real chapter page: producing
    // one from scratch would take an lz-string compressor, and with the
    // dictionary size declared as 0 its words are never consulted anyway.
    let real = load_test_html("comic_40811_chapter_1.html");
    let payload = RE_CHAPTER_DATA
        .captures(&real)
        .expect("real page carries packed data")[4]
        .to_string();

    let frame = r#"SMH.imgData({"sl":{"e":"123","m":"abc"},"cname":"It\'s \\"fine\\"","path":"/comic/","files":["01.jpg"]}).preInit();"#;
    let page = format!(
        "<script>eval(function(p,a,c,k,e,d){{}}('{frame}',10,0,'{payload}'.split('|'),0,{{}}))</script>"
    );

    let chapter =
        parse_chapter_html(&page).expect("escaped quotes in the frame must not abort the parse");
    assert_eq!(chapter.path, "/comic/");
    assert_eq!(chapter.files, vec!["01.jpg".to_string()]);
    assert_eq!(chapter.sl.m, "abc");
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
    assert_eq!(
        parse_chapter_selection(" 2 , 4 - 5 ", 10),
        Some(vec![1, 3, 4])
    );
    assert_eq!(
        parse_chapter_selection("1-5,3-7", 10),
        Some(vec![0, 1, 2, 3, 4, 5, 6])
    );
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
    // stdin closed immediately: must error out instead of looping forever.
    // Both prompts read through the one prompt_until_valid loop, so this
    // covers the comic selection prompt as well.
    let mut input = std::io::Cursor::new("");
    assert_input_closed(prompt_for_chapters(&mut input, 10));
}

#[test]
fn test_prompt_for_chapters_eof_after_invalid_input() {
    // Invalid input followed by EOF: must error out after the retry
    let mut input = std::io::Cursor::new("999\n");
    assert_input_closed(prompt_for_chapters(&mut input, 10));
}

#[test]
fn test_prompt_for_comic_selection_retry_on_invalid() {
    // Both prompts share one retry loop, so the comic side needs its own
    // coverage: out of bounds high, zero, non-numeric, then a valid pick that
    // must come back converted from 1-based input to a 0-based index.
    let mut input = std::io::Cursor::new("6\n0\nabc\n3\n");
    assert_eq!(prompt_for_comic_selection(&mut input, 5).unwrap(), 2);
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
fn test_illegal_chars_windows_forbidden() {
    // Every character Windows forbids in file names — \, /, :, *, ?, ", <, >, |
    // plus all of the C0 controls and DEL — becomes exactly one `_`. The list
    // is written out here rather than read from ILLEGAL_NAME_CHARS, so that a
    // character dropped from the constant is caught instead of agreed with.
    let forbidden = ('\0'..='\x1f')
        .chain(['\x7f'])
        .chain(['\\', '/', ':', '*', '?', '"', '<', '>', '|']);

    for ch in forbidden {
        assert_eq!(
            sanitize(&format!("file{ch}name")),
            "file_name",
            "Character {:?} should be replaced",
            ch
        );
    }
}

#[test]
fn test_illegal_chars_legal_characters_preserved() {
    // Everything sanitize does *not* replace must survive untouched: the
    // punctuation that is legal in a file name, and every non-ASCII script a
    // comic title can be written in. None of these has anything else for
    // sanitize to trim, cut or break up, so each must come back as it went in.
    let test_cases = [
        "file-name",      // Hyphen
        "file_name",      // Underscore
        "file.name",      // Dot
        "file (1)",       // Parentheses, space
        "file[backup]",   // Brackets
        "file@home",      // @
        "file&name",      // &
        "file+name",      // +
        "file=name",      // =
        "file name",      // Space
        "漫畫標題",       // Chinese
        "マンガ",         // Japanese
        "만화",           // Korean
        "file_🎯name",    // Emoji
        "file\u{80}name", // C1 control: legal, and past the ASCII controls
    ];

    for input in test_cases {
        assert_eq!(
            sanitize(input),
            input,
            "Legal filename characters should be preserved: {}",
            input
        );
    }
}

#[test]
fn test_compress_chapter_file_ordering() {
    // Page order comes from the caller's list, never from the directory. The
    // list below is deliberately in neither ascending nor descending name
    // order: a list that happened to be sorted would be reproduced by a
    // directory listing too, and the test would prove nothing. Each page holds
    // its own name as content, so an entry carrying the right name over another
    // page's bytes is caught as well.
    use std::fs::File;

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();

    let pages: Vec<String> = [
        "20_page.jpg",
        "01_page.jpg",
        "10_page.jpg",
        "03_page.jpg",
        "02_page.jpg",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for filename in &pages {
        std::fs::write(chapter_dir.join(filename), filename).unwrap();
    }

    let zip_path = test_dir.join("test.cbz");

    // Call the actual compress_chapter method
    Comic::compress_chapter(&chapter_dir, &pages, &zip_path).unwrap();

    // Verify zip file was created and directory removed
    assert!(zip_path.exists());
    assert!(!chapter_dir.exists());

    // Verify the order and content of the files inside the zip
    let file = File::open(&zip_path).unwrap();
    let mut archive = zip::read::ZipArchive::new(file).unwrap();

    assert_eq!(archive.len(), pages.len());
    for (i, page) in pages.iter().enumerate() {
        let mut zip_file = archive.by_index(i).unwrap();
        assert_eq!(
            zip_file.name(),
            page.as_str(),
            "File at index {} should be {}",
            i,
            page
        );
        let mut content = String::new();
        zip_file.read_to_string(&mut content).unwrap();
        assert_eq!(&content, page, "Content of {}", page);
    }
}

#[test]
fn test_compress_chapter_ignores_stale_files_from_previous_run() {
    // A chapter that gained pages between runs is re-downloaded under a wider
    // zero padding, leaving the old narrower names behind. Packing the
    // directory listing would both duplicate those pages and misplace them,
    // because '0' sorts before '_': "0_page.jpg" lands after "09_page.jpg".

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();

    // Stale single-digit names from the interrupted 9-page run, and the .part
    // its interruption left behind.
    for i in 0..9 {
        std::fs::write(chapter_dir.join(format!("{}_page.jpg", i)), b"stale").unwrap();
    }
    std::fs::write(chapter_dir.join("9_page.jpg.part"), b"partial").unwrap();
    // The current 12-page run.
    let pages: Vec<String> = (0..12).map(|i| format!("{:02}_page.jpg", i)).collect();
    for filename in &pages {
        std::fs::write(chapter_dir.join(filename), b"fresh").unwrap();
    }

    let zip_path = test_dir.join("test.cbz");
    Comic::compress_chapter(&chapter_dir, &pages, &zip_path).unwrap();

    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    assert_eq!(names, pages, "stale pages must not leak into the archive");
}

#[test]
fn test_compress_chapter_missing_page_is_error() {
    // A page named in the list but absent from disk must fail loudly: silently
    // dropping it would publish a .cbz with a hole that is then cached forever.

    let temp_dir = TempDir::new().unwrap();
    let test_dir = temp_dir.path();

    let chapter_dir = test_dir.join("chapter");
    std::fs::create_dir_all(&chapter_dir).unwrap();
    std::fs::write(chapter_dir.join("0_page.jpg"), b"image data").unwrap();

    let pages = vec!["0_page.jpg".to_string(), "1_page.jpg".to_string()];
    let zip_path = test_dir.join("test.cbz");

    assert!(Comic::compress_chapter(&chapter_dir, &pages, &zip_path).is_err());
    // The chapter must stay unfinished so a later run can retry it, with no
    // half-written archive left beside it.
    assert!(!zip_path.exists());
    assert!(!test_dir.join("test.cbz.part").exists());
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
    // ...but at either end the ones that are whitespace are trimmed first, not
    // kept as underscores the title never showed.
    assert_eq!(sanitize("第01話\n"), "第01話");
    assert_eq!(sanitize("\t第01話\r\n"), "第01話");
}

#[test]
fn test_sanitize_bounds_the_length() {
    // Anything at or below the budget is left exactly as it was.
    let fits = "a".repeat(MAX_NAME_BYTES);
    assert_eq!(sanitize(&fits), fits);

    // Past it, the name is cut to the budget.
    let long = "a".repeat(MAX_NAME_BYTES + 50);
    assert_eq!(sanitize(&long).len(), MAX_NAME_BYTES);

    // The cut lands on a character boundary rather than splitting a multi-byte
    // character — slicing one in half would panic. The leading ASCII byte puts
    // every following boundary at an offset the budget does not fall on, so
    // this only passes if the cut actually walks back to one.
    let cjk = format!("x{}", "漢".repeat(100));
    let cut = sanitize(&cjk);
    assert!(cut.len() <= MAX_NAME_BYTES);
    assert!(
        cjk.starts_with(&cut),
        "the cut must be a prefix of the input"
    );
    assert_eq!(cut.chars().count(), 1 + (MAX_NAME_BYTES - 1) / 3);

    // A cut that stops right after a dot must not leave it dangling: Windows
    // rejects a name ending in one.
    let dotted = format!("{}.{}", "a".repeat(MAX_NAME_BYTES - 1), "b".repeat(10));
    assert_eq!(sanitize(&dotted), "a".repeat(MAX_NAME_BYTES - 1));

    // A name that is nothing but dots once cut still falls back to "_".
    assert_eq!(sanitize(&".".repeat(MAX_NAME_BYTES + 50)), "_");
}

#[test]
fn test_sanitize_breaks_up_windows_device_names() {
    // A book or chapter named after a device cannot be created on Windows, and
    // it fails again on every later run — the comic would never be downloadable
    // at all. Bare, in either case, and with an extension, which is no escape:
    // Windows resolves the name from the stem alone.
    assert_eq!(sanitize("NUL"), "NUL_");
    assert_eq!(sanitize("nul"), "nul_");
    assert_eq!(sanitize("CON.txt"), "CON_.txt");
    assert_eq!(sanitize("com1"), "com1_");
    assert_eq!(sanitize("LPT9.tar.gz"), "LPT9_.tar.gz");

    // The check runs after the trims, which can expose a stem that was not the
    // whole name on the way in.
    assert_eq!(sanitize("NUL. "), "NUL_");
    assert_eq!(sanitize("  aux  "), "aux_");

    // A name that merely starts with one is left alone: the stem has to match
    // in full.
    assert_eq!(sanitize("CONAN"), "CONAN");
    assert_eq!(sanitize("NUL2"), "NUL2");
    assert_eq!(sanitize("com10"), "com10");
    assert_eq!(sanitize("第01話"), "第01話");

    // The `_` is inserted after the length cut, so a reserved stem with a long
    // tail comes back one byte over the budget. Cutting to make room would put
    // the reserved name straight back.
    let long = sanitize(&format!("NUL.{}", "x".repeat(200)));
    assert_eq!(long.len(), MAX_NAME_BYTES + 1);
    assert!(long.starts_with("NUL_."), "got {}", long);
}

#[test]
fn test_sanitized_names_compose_into_a_legal_component() {
    // The budget exists for the longest component the program builds: a
    // chapter's archive while it is still being written, which spends two
    // sanitized names at once under the suffix write_atomic appends, and 255
    // bytes is what ext4 and APFS allow per component. The worst case is two
    // device-name stems, each costing the extra byte sanitize spends to break
    // them up. Every other name the program builds — the finished archive, a
    // page file — is shorter than this one, so it is the only one checked.
    let book = sanitize(&format!("NUL.{}", "a".repeat(200)));
    let chapter = sanitize(&format!("CON.{}", "b".repeat(200)));
    let part_name = format!("{}_{}.cbz.part", book, chapter);
    assert!(
        part_name.len() <= 255,
        "partial archive name is {} bytes",
        part_name.len()
    );
}

#[test]
fn test_image_url_anchors_the_page_path_on_the_tunnel() {
    let comic = test_comic("https://i.hamreus.com", Path::new("."));

    // The ordinary shape: an absolute path from the chapter JSON plus a file
    // name from its `files` array.
    assert_eq!(
        comic
            .image_url("/ps3/l/foo/", "001.jpg.webp")
            .unwrap()
            .as_str(),
        "https://i.hamreus.com/ps3/l/foo/001.jpg.webp"
    );

    // A path arriving without its leading slash must still land on the tunnel.
    // Concatenation produced the host "i.hamreus.comps3" here.
    assert_eq!(
        comic
            .image_url("ps3/l/foo/", "001.jpg.webp")
            .unwrap()
            .as_str(),
        "https://i.hamreus.com/ps3/l/foo/001.jpg.webp"
    );

    // A protocol-relative "//host" is a path, not a new host: the page does not
    // get to redirect the download somewhere else.
    let url = comic.image_url("//evil.example/", "001.jpg.webp").unwrap();
    assert_eq!(url.host_str(), Some("i.hamreus.com"));
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/evil.example/001.jpg.webp"
    );

    // A backslash is the protocol-relative case in disguise: `Url` follows the
    // WHATWG rules, where it separates a path for special schemes, so the
    // leading slash this function anchors turns `\host` into `/\host` — which
    // parses as an authority exactly as `//host` does. Encoded, it stays a
    // path segment on the tunnel.
    let url = comic.image_url("\\evil.example/", "001.jpg.webp").unwrap();
    assert_eq!(url.host_str(), Some("i.hamreus.com"));
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/%5Cevil.example/001.jpg.webp"
    );

    // Tab, LF and CR are deleted by the WHATWG parser before it reads the
    // input, so one sitting in front of a slash hid it from the trim of
    // leading slashes and then vanished, leaving `//host` behind — with or
    // without a slash of the page's own ahead of it. Encoded, it stays a path
    // segment on the tunnel.
    for (c, encoded) in [('\t', "%09"), ('\n', "%0A"), ('\r', "%0D")] {
        for path in [format!("{c}/evil.example/"), format!("/{c}/evil.example/")] {
            let url = comic.image_url(&path, "001.jpg.webp").unwrap();
            assert_eq!(url.host_str(), Some("i.hamreus.com"), "path {path:?}");
            assert_eq!(
                url.as_str(),
                format!("https://i.hamreus.com/{encoded}/evil.example/001.jpg.webp"),
                "path {path:?}"
            );
        }
    }

    // `?` and `#` end a path rather than sitting in one, so a page could
    // otherwise cut the request short of the directory it named.
    let url = comic.image_url("/ps3/l/foo?a=b/", "001.jpg.webp").unwrap();
    assert_eq!(url.query(), None);
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/ps3/l/foo%3Fa=b/001.jpg.webp"
    );

    let url = comic.image_url("/ps3/l/foo#frag/", "001.jpg.webp").unwrap();
    assert_eq!(url.fragment(), None);
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/ps3/l/foo%23frag/001.jpg.webp"
    );

    // The file name is read out of the same JSON, and all three are encoded
    // there too.
    let url = comic.image_url("/ps3/l/foo/", "001.jpg?x#y\\z").unwrap();
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/ps3/l/foo/001.jpg%3Fx%23y%5Cz"
    );

    // Behind a bare "/" directory, the file name alone reaches the anchored
    // slash, so a tab there is the same escape as one in the path.
    let url = comic
        .image_url("/", "\t/evil.example/001.jpg.webp")
        .unwrap();
    assert_eq!(url.host_str(), Some("i.hamreus.com"));
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/%09/evil.example/001.jpg.webp"
    );

    // Raw UTF-8 comes back percent-encoded, as it does for page links. An
    // escape the page already wrote rides through unchanged rather than being
    // encoded a second time into `%25`.
    let url = comic
        .image_url("/ps3/%E7%AC%AC01/", "001.jpg.webp")
        .unwrap();
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/ps3/%E7%AC%AC01/001.jpg.webp"
    );

    let url = comic.image_url("/ps3/第01話/", "001.jpg.webp").unwrap();
    assert_eq!(
        url.as_str(),
        "https://i.hamreus.com/ps3/%E7%AC%AC01%E8%A9%B1/001.jpg.webp"
    );
}

#[test]
fn test_looks_like_image_accepts_the_served_formats() {
    // Enough of a header for each of the two signatures the allow list carries.
    assert!(looks_like_image(b"\xFF\xD8\xFF\xE0\x00\x10JFIF"));
    assert!(looks_like_image(b"RIFF\x24\x00\x00\x00WEBP"));
}

#[test]
fn test_looks_like_image_rejects_non_images() {
    // What a 200 anti-hotlink or interstitial response actually carries.
    assert!(!looks_like_image(b"<!DOCTYPE html"));
    assert!(!looks_like_image(b"<html><body>4"));
    assert!(!looks_like_image(b"{\"error\":\"403\""));
    // A RIFF container that is not WebP, and a truncated one that never gets
    // far enough to prove it is.
    assert!(!looks_like_image(b"RIFF\x24\x00\x00\x00WAVE"));
    assert!(!looks_like_image(b"RIFF\x24\x00"));
    // Nothing at all, which is what an empty 200 body reads as.
    assert!(!looks_like_image(b""));
    // Image formats outside the two the tunnel serves are turned away as well.
    // The list is deliberately narrow: picking one up has to be a deliberate
    // edit, not something that slips in behind a generic "is an image" test.
    assert!(!looks_like_image(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d"));
    assert!(!looks_like_image(b"GIF89a\x01\x00\x01\x00\x00\x00"));
    assert!(!looks_like_image(b"\x00\x00\x00\x20ftypavif"));
}

#[test]
fn test_download_fetches_missing_pages_and_keeps_existing_ones() {
    // Eleven pages put the zero padding at two digits. Nine of them are on disk
    // from an earlier run, so only the other two may be requested, each with
    // the chapter's `e`/`m` query — and the returned list, which is what
    // compress_chapter packs, still has to name all eleven in reading order.
    let files = [
        "a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg", "g.jpg", "h.jpg", "i.jpg", "j.jpg",
        "k.jpg",
    ];
    let names: Vec<String> = files
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{:02}_{}", i, f))
        .collect();
    let missing = [3, 10];

    // Each response carries the path it was requested under, so a page saved
    // under another page's name shows up as the wrong content.
    let (tunnel, server) = serve(missing.len(), |line| {
        let target = line.split(' ').nth(1).unwrap_or_default();
        let path = target.split('?').next().unwrap_or_default();
        http_200(&[JPEG_SOI, path.as_bytes()].concat())
    });

    let fx = DownloadFixture::new(&tunnel);
    let chapter_dir = fx.dir.path();
    for (i, name) in names.iter().enumerate() {
        if !missing.contains(&i) {
            std::fs::write(chapter_dir.join(name), "already here").unwrap();
        }
    }

    let returned = fx.download(&files).expect("both missing pages are served");

    assert_eq!(returned, names);
    assert_eq!(fx.bar.position(), files.len() as u64);
    for (i, name) in names.iter().enumerate() {
        let expected = if missing.contains(&i) {
            [JPEG_SOI, format!("/{}", files[i]).as_bytes()].concat()
        } else {
            b"already here".to_vec()
        };
        assert_eq!(
            std::fs::read(chapter_dir.join(name)).unwrap(),
            expected,
            "Content of {}",
            name
        );
    }
    // Nothing but the pages themselves: no `.part` left behind by either download.
    assert_eq!(std::fs::read_dir(chapter_dir).unwrap().count(), files.len());

    assert_eq!(
        server.join().unwrap(),
        vec![
            "GET /d.jpg?e=test_e&m=test_m HTTP/1.1".to_string(),
            "GET /k.jpg?e=test_e&m=test_m HTTP/1.1".to_string(),
        ]
    );
}

#[test]
fn test_download_rejects_non_image_body() {
    // An anti-hotlink page answering 200 gets past error_for_status. Accepting
    // it would publish it under the page's final name, where every later run
    // skips it as already downloaded and compress_chapter seals it into the
    // .cbz as a page.
    let body = b"<!DOCTYPE html><html><body>forbidden</body></html>";
    let (tunnel, server) = serve(1, move |_| http_200(body));
    let fx = DownloadFixture::new(&tunnel);
    let chapter_dir = fx.dir.path();

    assert_err_contains(fx.download(&["test.jpg"]), "is not an image");

    // Neither the final name nor a leftover .part may survive the rejection.
    assert!(!chapter_dir.join("0_test.jpg").exists());
    assert!(!chapter_dir.join("0_test.jpg.part").exists());

    server.join().unwrap();
}

#[test]
fn test_download_incomplete_file() {
    // Declare 100 bytes, send 50, then close. The body opens with a real JPEG
    // signature so that it gets past the signature check that runs first.
    let (tunnel, server) = serve(1, |_| {
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(JPEG_SOI);
        response.extend_from_slice(&[0u8; 46]);
        response
    });
    let fx = DownloadFixture::new(&tunnel);
    let chapter_dir = fx.dir.path();

    let err_msg = fx
        .download(&["test.jpg"])
        .expect_err("a truncated body must not be accepted as a page")
        .to_string();
    // The response states its length, so the transport notices the short body
    // on its own and reports it before download_images' own length check gets
    // to look: that check is not what this test reaches, and the message is
    // whichever of these reqwest produces. What the test does pin down is that
    // the truncated page is not published.
    assert!(
        err_msg.contains("Incomplete download")
            || err_msg.contains("end of file")
            || err_msg.contains("UnexpectedEof"),
        "Error message should mention incomplete download or EOF, got: {}",
        err_msg
    );

    // Neither the final name ("0_test.jpg", width 1) nor a leftover .part may
    // survive the failure.
    assert!(
        !chapter_dir.join("0_test.jpg").exists(),
        "Final file should not exist after incomplete download"
    );
    assert!(!chapter_dir.join("0_test.jpg.part").exists());

    server.join().unwrap();
}

#[test]
fn test_download_resume_logic() {
    let fx = DownloadFixture::new("http://invalid-host-should-not-be-reached");

    // Create a dummy file that matches what download_images expects
    // For 1 file, width is 1. Filename: 0_test.jpg
    let existing_file = fx.dir.path().join("0_test.jpg");
    let original_content = "already here";
    fs::write(&existing_file, original_content).unwrap();

    // If the logic is correct, it will see 0_test.jpg exists and skip network calls.
    // If it attempts to download, it will fail because the tunnel host is invalid.
    let result = fx.download(&["test.jpg"]);

    assert!(
        result.is_ok(),
        "Should skip existing file and return Ok, but got error"
    );
    // Skipped pages still have to be reported, or compress_chapter would omit them.
    assert_eq!(result.unwrap(), vec!["0_test.jpg".to_string()]);

    // Verify content remains unchanged and progress bar incremented
    let content = fs::read_to_string(&existing_file).unwrap();
    assert_eq!(content, original_content);
    assert_eq!(fx.bar.position(), 1);
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

/// A saved search result page, together with the URL it was fetched from —
/// the base `parse_search_results` resolves the pager's href against. The
/// fixtures are named after the path they were saved from.
fn load_search_page(filename: &str) -> (String, Url) {
    let url = Url::parse(&format!("https://tw.manhuagui.com/s/{filename}")).unwrap();
    (load_test_html(filename), url)
}

#[test]
fn test_parse_search_results_page() {
    let (html, url) = load_search_page("金田一.html");
    let (results, next_page) = parse_search_results(&html, &url);

    assert_eq!(results.len(), 10);
    assert_eq!(results[0].title, "金田一爸爸事件簿");
    assert_eq!(results[0].comic_id, 54544);
    assert_eq!(results[9].title, "金田一少年事件簿 鍊金術殺人事件");
    assert_eq!(results[9].comic_id, 4825);

    // The href exactly as the pager wrote it, raw UTF-8 and all.
    // test_join_url_percent_encodes_pager_hrefs resolves this same href against
    // this same page.
    assert_eq!(next_page.as_deref(), Some("/s/金田一_p2.html"));
}

#[test]
fn test_parse_search_results_last_page() {
    let (html, url) = load_search_page("金田一_p3.html");
    let (results, next_page) = parse_search_results(&html, &url);

    assert_eq!(results.len(), 8); // 28 total - 10 (page 1) - 10 (page 2) = 8
    assert!(next_page.is_none());
}

#[test]
fn test_parse_search_results_next_link_must_lead_to_a_page() {
    // A pager can disable "下一頁" by pointing it at `javascript:;` instead of
    // leaving it out, and that has to read as the last page: passed on, the
    // user would be asked to press SPACE for a page that is then refused. The
    // scheme is matched case-insensitively, as a URL's is. A document-relative
    // href carries no scheme of its own and still counts as a page.
    //
    // Pointing it back at the page itself disables it just as well, and passed
    // on, every SPACE would fetch and list the same page again. That is decided
    // on what the href resolves to, not on how it is spelled: a bare fragment,
    // an empty href, padding the parser strips, the page's own path written
    // raw or relative, and a fragment on any of them all name this page.
    let (page, url) = load_search_page("金田一.html");
    let next_link = r#"href="/s/金田一_p2.html" style="margin-right:5px;">下一頁"#;
    assert!(
        page.contains(next_link),
        "the fixture's next link has changed"
    );

    for (href, expected) in [
        ("javascript:;", None),
        ("JavaScript:void(0)", None),
        ("金田一_p2.html", Some("金田一_p2.html")),
        ("#", None),
        ("", None),
        (" #", None),
        ("/s/金田一.html", None),
        ("金田一.html#top", None),
    ] {
        let html = page.replace(
            next_link,
            &format!(r#"href="{href}" style="margin-right:5px;">下一頁"#),
        );
        let (results, next_page) = parse_search_results(&html, &url);
        assert_eq!(results.len(), 10, "results must not depend on the pager");
        assert_eq!(next_page.as_deref(), expected, "next link href {href}");
    }
}

#[test]
fn test_join_url_percent_encodes_pager_hrefs() {
    // The pager on a real search page emits hrefs with raw UTF-8 in them (see
    // the div.pager block in 金田一.html). They must come back out as ASCII:
    // the resolved URL is reused as the `referer` header of the following
    // request, and header values may not carry non-ASCII bytes. Joined against
    // the result page, the way interactive_search resolves a pager href.
    let from_page = Url::parse("https://tw.manhuagui.com/s/金田一.html").unwrap();
    let url = join_url(&from_page, "/s/金田一_p2.html").unwrap();
    assert_eq!(
        url.as_str(),
        "https://tw.manhuagui.com/s/%E9%87%91%E7%94%B0%E4%B8%80_p2.html"
    );
}

#[test]
fn test_resolve_url_leaves_encoded_and_absolute_hrefs_alone() {
    // An already-encoded href must not be double-encoded ('%' is path-safe)...
    assert_eq!(
        resolve_url("/s/%E9%87%91.html").unwrap().as_str(),
        "https://tw.manhuagui.com/s/%E9%87%91.html"
    );
    // ...and an absolute href must replace the base rather than be appended to
    // it, which is what plain string concatenation used to do.
    assert_eq!(
        resolve_url("https://tw.manhuagui.com/s/x_p2.html")
            .unwrap()
            .as_str(),
        "https://tw.manhuagui.com/s/x_p2.html"
    );
}

#[test]
fn test_comic_url_ends_with_a_slash() {
    // The site answers the slashless form with a 404, so the slash is what
    // makes the landing page reachable at all.
    assert_eq!(
        comic_url(26897).unwrap().as_str(),
        "https://tw.manhuagui.com/comic/26897/"
    );
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
    let (title, chapters) = parse_comic_html(&html).expect("Failed to parse comic HTML");

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
        assert_eq!(
            names[i], expected,
            "Position {} should be {}, got {}",
            i, expected, names[i]
        );
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
