use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue},
    Url,
};
use scraper::{Html, Selector};
use serde::Deserialize;
use regex::Regex;
use rand::Rng;
use std::{
    fs,
    io::{self, Write},
    num::ParseIntError,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::Duration,
};
use thiserror::Error;
use zip::{result::ZipError, write::FileOptions, CompressionMethod, ZipWriter};
use std::sync::LazyLock;

const HOST: &str = "https://tw.manhuagui.com";
const TUNNEL_CHANNELS: [&str; 3] = ["i", "eu", "us"];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `HOST` parsed once, as the base every site-relative link is resolved against.
/// Parsing also normalizes it to a trailing slash, which is what the site root
/// referer needs to be.
static HOST_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(HOST).expect("HOST is a valid absolute URL"));

/// `--tunnel` help text, generated from `TUNNEL_CHANNELS` so the option's
/// documentation cannot drift when the channel list changes.
static TUNNEL_HELP: LazyLock<String> = LazyLock::new(|| {
    let channels = TUNNEL_CHANNELS
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{i}={c}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("Tunnel line: {channels}")
});

static RE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:https?://(?:[\w\.]+\.)?manhuagui\.com)?/comic/)?(\d+)\b").unwrap()
});
static RE_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\w+\b").unwrap());
static RE_JSON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\{.*?\})\)").unwrap());
static RE_CHAPTER_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"}\('\s*(.*?)',(\d+),(\d+),'([\w+/=]+)'").unwrap());
static RE_ILLEGAL_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r##"[\x00-\x1f\x7f\\/:*?"<>|]"##).unwrap());

static SEL_COMICS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.book-result ul li.cf").unwrap());
static SEL_LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.bcover").unwrap());
static SEL_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".book-title h1").unwrap());
/// A chapter link inside a `.chapter-list`. Both attributes are required by the
/// selector so that non-chapter anchors — the `<a id="v1" href="javascript:;">`
/// pager entries that sit in a sibling block today, ad links, "more" links — are
/// simply not matched, rather than aborting the whole book's parse.
static SEL_CHAPTER_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href][title]").unwrap());
static SEL_PAGER_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.pager a").unwrap());
static SEL_VIEWSTATE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("input#__VIEWSTATE").unwrap());
static SEL_CHAPTER_LIST: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".chapter-list").unwrap());

/// Per-chapter download bar. The template is fixed, so parse it once instead of
/// re-parsing (and re-`unwrap`ping) it for every chapter.
static BAR_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::default_bar()
        .template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
        )
        .unwrap()
        .progress_chars("#>-")
});

#[derive(Error, Debug)]
enum AppError {
    #[error("Invalid manhuagui URL or ID")]
    InvalidUrl,
    #[error("Interrupted by Ctrl+C")]
    Interrupted,
    #[error("No comics found for '{0}'")]
    NoSearchResults(String),
    #[error("Content parsing error: {0}")]
    ContentParsing(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("Network request error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("JSON parsing error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Integer parsing error: {0}")]
    ParseInt(#[from] ParseIntError),
    #[error("Zip error: {0}")]
    Zip(#[from] ZipError),
}

type Result<T> = std::result::Result<T, AppError>;

/// Simple Manhuagui downloader in Rust
#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    /// Manhuagui URL or numeric ID
    #[clap(value_name = "URL", required_unless_present = "search", conflicts_with = "search")]
    url: Option<String>,
    /// Search keyword for comics
    #[clap(short, long)]
    search: Option<String>,
    #[clap(short, long, default_value_t = 0, help = TUNNEL_HELP.as_str(), value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(..TUNNEL_CHANNELS.len() as u64))]
    tunnel: usize,
    /// Delay between pages in milliseconds
    #[clap(short, long, default_value_t = 1000)]
    delay_ms: u64,
    /// Output directory
    #[clap(short, long, default_value = "Downloads")]
    output_dir: PathBuf,
}

/// Extract a comic ID from a bare number, an absolute manhuagui comic URL,
/// or a site-relative path like `/comic/12345/` (as found in search results).
///
/// The input is trimmed first, because `RE_ID` is anchored at the start of the
/// string: a pasted URL carrying a leading space, or an `href` attribute the
/// page wrote with surrounding whitespace (which a browser strips and the HTML
/// parser does not), would otherwise not match at all.
fn parse_id(s: &str) -> Option<usize> {
    RE_ID.captures(s.trim()).and_then(|c| c[1].parse().ok())
}

#[derive(Deserialize, Debug)]
struct ChapterStruct {
    sl: Sl,
    path: String,
    files: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct Sl {
    e: NumOrStr,
    m: String,
}

/// `sl.e` appears as either a number or a string in chapter data; accept both
/// and reject anything else at deserialization time.
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum NumOrStr {
    Num(serde_json::Number),
    Str(String),
}

impl std::fmt::Display for NumOrStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumOrStr::Num(n) => n.fmt(f),
            NumOrStr::Str(s) => s.fmt(f),
        }
    }
}

#[derive(Debug)]
struct SearchResult {
    title: String,
    comic_id: usize,
}

#[derive(Debug)]
struct Chapter {
    name: String,
    href: String,
    /// Section heading the chapter belongs to, e.g. "單行本" (volumes) or "單話" (single chapters)
    group: String,
}

struct Comic {
    client: Client,
    tunnel: String,
    delay: Duration,
    title: String,
    chapters: Vec<Chapter>,
    /// Sanitized title, used as the book directory name and zip name prefix.
    book_safe: String,
    book_dir: PathBuf,
}

/// The `.part` file `write_atomic` builds its output in, and the only thing
/// that constructs one — go through `write_atomic` rather than driving this
/// directly.
///
/// Dropping the guard without a successful `commit` deletes the partial file.
/// Without that, every exit between creating it and the final rename — a
/// stalled transfer, a short read, a page missing from disk, the rename itself
/// — would leave a stray `.part` behind, in a directory that is otherwise only
/// cleaned up on the success path.
struct PartFile {
    part: PathBuf,
    dst: PathBuf,
    committed: bool,
}

impl PartFile {
    /// Create `dst` with `.part` appended, truncating any leftover from an
    /// earlier run, and hand back the guard together with the open handle.
    fn create(dst: &Path) -> Result<(Self, fs::File)> {
        let mut part = dst.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let file = fs::File::create(&part)?;
        Ok((
            PartFile {
                part,
                dst: dst.to_path_buf(),
                committed: false,
            },
            file,
        ))
    }

    /// Move the finished file into place. The handle it was written through
    /// must be closed first: Windows can refuse to move a file that is still
    /// open. A failed rename leaves the guard armed, so the partial file is
    /// removed on the way out.
    fn commit(mut self) -> Result<()> {
        fs::rename(&self.part, &self.dst)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.part);
        }
    }
}

/// Produce `dst` through a `.part` file, so its final name only ever appears
/// once the content behind it is complete: `write` fills the handle, the handle
/// is closed, and only then is the file renamed into place. Anything `write`
/// rejects — a short read, a page missing from disk — must be reported from
/// inside it, because returning `Ok` is what publishes the file.
fn write_atomic(dst: &Path, write: impl FnOnce(&mut fs::File) -> Result<()>) -> Result<()> {
    let (part, mut file) = PartFile::create(dst)?;
    write(&mut file)?;
    // Close the handle before the rename: Windows can refuse to move a file
    // that is still open. `file` is bound after `part`, so on the error path
    // above it is dropped — and closed — first as well, before `part` removes
    // the partial file.
    drop(file);
    part.commit()
}

fn unpack_packed(
    frame: &str,
    a: usize,
    c: usize,
    data: &[&str],
) -> Result<ChapterStruct> {
    const DIGITS: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    // `encode` relies on the base being validated once up front.
    fn encode(mut value: usize, base: usize) -> String {
        if value == 0 {
            return "0".to_string();
        }
        let mut res = String::new();
        while value > 0 {
            let rem = value % base;
            res.insert(0, DIGITS.as_bytes()[rem] as char);
            value /= base;
        }
        res
    }
    if a < 2 || a > DIGITS.len() {
        return Err(AppError::ContentParsing(format!(
            "Base {} out of supported range (2..={})",
            a,
            DIGITS.len()
        )));
    }
    if c > data.len() {
        return Err(AppError::ContentParsing(format!(
            "Packed script dictionary size mismatch: expected {} words, got {}",
            c,
            data.len()
        )));
    }
    // Sized only after the check above, so a bogus `c` from a hostile page
    // cannot ask for a huge allocation up front.
    let mut dmap = std::collections::HashMap::with_capacity(c);
    // `take(c)` keeps the dictionary at the size the packed script declared
    // without indexing, so the length check above is the only thing standing
    // between a bogus `c` and an out-of-bounds access — there is no second
    // place to get it wrong.
    for (i, word) in data.iter().take(c).enumerate() {
        // An empty dictionary entry maps the word to itself, which is also
        // what the replacement below does for unknown words — skip it before
        // paying for the `encode` allocation.
        if !word.is_empty() {
            dmap.insert(encode(i, a), *word);
        }
    }
    let js = RE_WORD
        .replace_all(frame, |caps: &regex::Captures| {
            let key = caps.get(0).unwrap().as_str();
            dmap.get(key).copied().unwrap_or(key).to_string()
        })
        .into_owned();
    let caps = RE_JSON.captures(&js).ok_or_else(|| {
        AppError::ContentParsing("Could not find JSON data in unpacked script.".to_string())
    })?;
    let chapter: ChapterStruct = serde_json::from_str(&caps[1])?;
    // A chapter with no images would produce an empty .cbz that marks the
    // chapter as done forever; fail here so the user sees an error instead.
    if chapter.files.is_empty() {
        return Err(AppError::ContentParsing(
            "Chapter data contains no image files".to_string(),
        ));
    }
    Ok(chapter)
}

/// Make `s` usable as a single path component: replace characters that are
/// invalid in file names with `_`, then strip surrounding whitespace and
/// trailing dots. Windows rejects names ending in a dot or a space, and a name
/// consisting only of dots (`.`, `..`) would otherwise resolve to a directory
/// outside the intended one. Falls back to `_` when nothing usable is left.
fn sanitize(s: &str) -> String {
    let replaced = RE_ILLEGAL_CHARS.replace_all(s, "_");
    let trimmed = replaced.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

fn decode_lz_base64(data: &str, what: &str) -> Result<String> {
    lz_string::Decoder::new()
        .decode_base64(data)
        .map_err(|_| AppError::ContentParsing(format!("Failed to decode {}", what)))
}

/// Only the headers that are identical on *every* request belong here.
/// Anything that describes one kind of request — `accept`, `priority`, the
/// `sec-fetch-*` triple — is set at the call site instead: `fetch_html` sends
/// them as a top-level navigation, `download_images` as a cross-site image.
/// Keeping request-shaped values out of the defaults is what stops the image
/// fetch from having to override five of them to undo a document's.
fn build_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("accept-encoding", "gzip, deflate, br"),
        ("accept-language", "zh-TW;q=0.8,en-US,en;q=0.5,zh;q=0.3"),
        ("cache-control", "no-cache"),
        ("dnt", "1"),
        ("pragma", "no-cache"),
        ("sec-gpc", "1"),
        ("user-agent", "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"),
    ] {
        // Both halves are `&'static str` literals, so both conversions are
        // infallible in practice and panic on a typo rather than turning a bug
        // in this table into a runtime error the user has to interpret.
        headers.insert(key, HeaderValue::from_static(value));
    }
    // Without timeouts a connection that stalls after the handshake hangs the
    // download forever; the request timeout covers reading the response body,
    // which is where image downloads spend their time.
    Ok(Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

/// Resolve a link taken off a page against the site root.
///
/// The search pager emits site-relative hrefs that still carry raw UTF-8, e.g.
/// `/s/金田一_p2.html`. Joining percent-encodes them, which matters twice over:
/// the resolved URL is requested, and on the next iteration it is handed back as
/// the `referer` header, where non-ASCII bytes have no business being. Joining
/// also keeps an absolute href working instead of concatenating it onto `HOST`.
fn resolve_url(href: &str) -> Result<Url> {
    HOST_URL
        .join(href)
        .map_err(|e| AppError::ContentParsing(format!("Invalid URL '{href}': {e}")))
}

/// Fetch a page the way a browser fetches a top-level document; see
/// `build_client` for why the document-shaped headers live here rather than in
/// the client defaults.
fn fetch_html(client: &Client, url: &str, referer: &str) -> Result<String> {
    Ok(client
        .get(url)
        .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("cookie", "country=TW")
        .header("priority", "u=0, i")
        .header("referer", referer)
        .header("sec-fetch-dest", "document")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-user", "?1")
        .header("upgrade-insecure-requests", "1")
        .send()?
        .error_for_status()?
        .text()?)
}

/// Wait for a single key press in raw mode. Returns `Ok(true)` for SPACE and
/// `Ok(false)` for any other key. Raw mode swallows Ctrl+C instead of raising
/// SIGINT, so it is detected here and reported as `AppError::Interrupted`.
fn wait_for_space() -> Result<bool> {
    if terminal::enable_raw_mode().is_err() {
        return Ok(false);
    }
    let result = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('c' | 'C'),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            })) if modifiers.contains(KeyModifiers::CONTROL) => break Err(AppError::Interrupted),
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                kind: KeyEventKind::Press,
                ..
            })) => break Ok(true),
            Ok(Event::Key(KeyEvent {
                kind: KeyEventKind::Press,
                ..
            })) => break Ok(false),
            Err(_) => break Ok(false),
            _ => {}
        }
    };
    let _ = terminal::disable_raw_mode();
    result
}

fn search_result_from_item(li: scraper::ElementRef<'_>) -> Option<SearchResult> {
    let link = li.select(&SEL_LINK).next()?;
    let comic_id = parse_id(link.value().attr("href")?)?;
    let title = link.value().attr("title")?;
    Some(SearchResult {
        title: title.to_string(),
        comic_id,
    })
}

/// Extract one page of search hits plus the href of the "next page" pager link,
/// if the page has one. A page with no recognizable results is not an error
/// here — `interactive_search` decides that only after the last page.
fn parse_search_results(html: &str) -> (Vec<SearchResult>, Option<String>) {
    let document = Html::parse_document(html);

    let results: Vec<SearchResult> = document
        .select(&SEL_COMICS)
        .filter_map(search_result_from_item)
        .collect();

    let next_page = document.select(&SEL_PAGER_LINKS)
        .find(|a| a.text().collect::<String>().trim() == "下一頁")
        .and_then(|a| a.value().attr("href"))
        .map(|s| s.to_string());

    (results, next_page)
}

/// The section heading of a chapter list is the nearest `h4` among its
/// preceding siblings (other elements like the pager or tip blocks may sit
/// in between).
fn group_for_list(list_elem: scraper::ElementRef<'_>) -> String {
    list_elem
        .prev_siblings()
        .filter_map(scraper::ElementRef::wrap)
        .find(|e| e.value().name() == "h4")
        .map(|h| h.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Chapters".to_string())
}

/// Collect every chapter, tagged with the section heading it sits under.
///
/// A `.chapter-list` holds one `<ul>` per pager page (only the last one is
/// rendered with `style="display:block"`; the pager swaps between them in the
/// browser), and the two nesting levels are ordered *differently*: the `ul`s
/// run oldest block first, while the entries inside each `ul` run newest
/// first. comic_1128's 單行本 section is
/// `[ul(第22卷 … 第01卷), ul(第112卷 … 第23卷)]`, so reversing each `ul` on its
/// own while keeping the `ul` order yields a continuous 第01卷 … 第112卷.
///
/// Do not collapse this into a single `ul a` selector and do not hoist the
/// reverse up to the whole `.chapter-list` — either change silently scrambles
/// chapter order across pager boundaries. For the same reason the `ul`s are
/// taken as direct children rather than with a descendant selector: a nested
/// `ul` would otherwise be visited twice, once through its parent block and
/// once as a block of its own, duplicating its links and splitting the
/// enclosing block's reverse into pieces.
fn extract_chapters_with_groups(document: &Html) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    for list_elem in document.select(&SEL_CHAPTER_LIST) {
        let group = group_for_list(list_elem);
        let uls = list_elem
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .filter(|e| e.value().name() == "ul");
        for ul_elem in uls {
            let start = chapters.len();
            chapters.extend(ul_elem.select(&SEL_CHAPTER_LINK).filter_map(|element| {
                let element = element.value();
                Some(Chapter {
                    name: element.attr("title")?.to_string(),
                    href: element.attr("href")?.to_string(),
                    group: group.clone(),
                })
            }));
            // Entries within one `ul` are listed newest-first; reverse into
            // reading order. Reversing just this `ul`'s slice is what keeps
            // the pager blocks in the order described above.
            chapters[start..].reverse();
        }
    }

    chapters
}

impl Comic {
    /// Fetch a comic's landing page and build the download context around it.
    ///
    /// `id` stays a separate argument rather than being read off `args`: it may
    /// have come from `--search` instead of the positional URL.
    fn new(id: usize, client: Client, args: &Args) -> Result<Self> {
        let url = resolve_url(&format!("/comic/{id}"))?;
        let res = fetch_html(&client, url.as_str(), HOST_URL.as_str())?;
        let (title, chapters) = Self::parse_comic_html(&res)?;
        let book_safe = sanitize(&title);
        let book_dir = args.output_dir.join(&book_safe);
        Ok(Comic {
            client,
            tunnel: format!("https://{}.hamreus.com", TUNNEL_CHANNELS[args.tunnel]),
            delay: Duration::from_millis(args.delay_ms),
            title,
            chapters,
            book_safe,
            book_dir,
        })
    }

    fn parse_comic_html(html: &str) -> Result<(String, Vec<Chapter>)> {
        let document = Html::parse_document(html);
        let title = document
            .select(&SEL_TITLE)
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AppError::ContentParsing("Could not find title".to_string()))?;

        let mut chapters = extract_chapters_with_groups(&document);

        // A gated page ships the real chapter list in the __VIEWSTATE blob.
        // A decode failure must not abort the parse: the input may be an
        // unrelated ASP.NET view state that merely happens to use that id, and
        // the "no chapters" error below describes the situation far better
        // than a decoder complaint would.
        if chapters.is_empty() {
            if let Some(decoded) = document
                .select(&SEL_VIEWSTATE)
                .next()
                .and_then(|e| e.value().attr("value"))
                .and_then(|vs_val| decode_lz_base64(vs_val, "__VIEWSTATE").ok())
            {
                chapters = extract_chapters_with_groups(&Html::parse_fragment(&decoded));
            }
        }

        if chapters.is_empty() {
            return Err(AppError::ContentParsing(
                "No chapters found (page layout changed or content is gated)".to_string(),
            ));
        }

        Ok((title, chapters))
    }

    fn get_chapter(&self, url: &str) -> Result<ChapterStruct> {
        let text = fetch_html(&self.client, url, HOST_URL.as_str())?;
        Self::parse_chapter_html(&text)
    }

    fn parse_chapter_html(html: &str) -> Result<ChapterStruct> {
        let caps = RE_CHAPTER_DATA
            .captures(html)
            .ok_or_else(|| AppError::ContentParsing("Could not parse chapter data".to_string()))?;

        let frame = &caps[1];
        let a: usize = caps[2].parse()?;
        let c: usize = caps[3].parse()?;
        let data_b64 = &caps[4];

        let data_dec = decode_lz_base64(data_b64, "base64 chapter data")?;
        let data: Vec<&str> = data_dec.split('|').collect();
        unpack_packed(frame, a, c, &data)
    }

    /// Download every page of `chap` into `chapter_dir`, skipping pages that are
    /// already there. Returns the page file names in reading order, which is what
    /// `compress_chapter` packs.
    fn download_images(&self, chap: &ChapterStruct, chapter_dir: &Path, bar: &ProgressBar, chapter_url: &str) -> Result<Vec<String>> {
        let width = chap.files.len().saturating_sub(1).to_string().len();
        let e_str = chap.sl.e.to_string();
        let mut names = Vec::with_capacity(chap.files.len());
        let mut needs_delay = false;
        for (i, file) in chap.files.iter().enumerate() {
            let url = format!("{}{}{}", self.tunnel, chap.path, file);
            let file_safe = sanitize(file);
            let fname = format!("{:0width$}_{}", i, file_safe, width = width);
            let dst = chapter_dir.join(&fname);
            names.push(fname);

            if dst.exists() {
                bar.inc(1);
                continue;
            }
            // Space out consecutive downloads; no delay before the first one
            // or after the last one.
            if needs_delay {
                thread::sleep(rand::rng().random_range(self.delay / 2..=self.delay * 3 / 2));
            }
            let mut resp = self
                .client
                .get(&url)
                .header("accept", "image/webp,image/apng,image/*,*/*;q=0.8")
                .header("priority", "u=4")
                .header("referer", chapter_url)
                .header("sec-fetch-dest", "image")
                .header("sec-fetch-mode", "no-cors")
                .header("sec-fetch-site", "cross-site")
                .query(&[("e", &e_str), ("m", &chap.sl.m)])
                .send()?
                .error_for_status()?;

            let content_length = resp.content_length();
            // The length check belongs inside the closure: returning `Ok` is
            // what renames the file into place, and a truncated page under the
            // final name would be skipped as finished by every later run.
            write_atomic(&dst, |out| {
                let bytes_written = io::copy(&mut resp, out)?;
                if let Some(expected) = content_length {
                    if bytes_written != expected {
                        return Err(AppError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("Incomplete download: expected {} bytes, got {}", expected, bytes_written),
                        )));
                    }
                }
                Ok(())
            })?;
            bar.inc(1);
            needs_delay = true;
        }
        Ok(names)
    }

    /// Pack exactly the pages `download_images` reported, in the order it
    /// reported them.
    ///
    /// The archive contents must come from that list and not from whatever
    /// `chapter_dir` happens to hold: the zero padding of a page's file name is
    /// derived from the chapter's total page count, so a chapter that gained
    /// pages since an interrupted run leaves stale files behind under a
    /// narrower width (`0_a.webp` next to a new `00_a.webp`). Reading the
    /// directory and sorting would fold those duplicates in — and sort them
    /// into the wrong places, since `'0' < '_'` puts `0_a.webp` after
    /// `09_a.webp` — producing a scrambled .cbz that is then cached forever.
    fn compress_chapter(chapter_dir: &Path, file_names: &[String], zip_path: &Path) -> Result<()> {
        write_atomic(zip_path, |zip_file| {
            let mut zip = ZipWriter::new(zip_file);
            let options = FileOptions::default().compression_method(CompressionMethod::Stored);

            for name in file_names {
                zip.start_file(name.as_str(), options)?;
                let mut file = fs::File::open(chapter_dir.join(name))?;
                io::copy(&mut file, &mut zip)?;
            }

            // Finish explicitly rather than leaving it to `ZipWriter`'s `Drop`,
            // which has nowhere to report a failure to write out the central
            // directory. The handle itself is owned — and closed — by
            // `write_atomic`.
            zip.finish()?;
            Ok(())
        })?;
        // The .cbz is already in place; failing to clean up the now-redundant
        // image directory must not report the chapter as failed. Warn instead.
        if let Err(e) = fs::remove_dir_all(chapter_dir) {
            eprintln!(
                "Warning: failed to remove temporary directory {}: {}",
                chapter_dir.display(),
                e
            );
        }
        Ok(())
    }

    fn download_chapter(&self, index: usize) -> Result<bool> {
        let Chapter { name, href, .. } = &self.chapters[index];
        let chap_safe = sanitize(name);
        // Chapter names are unique across the whole comic on manhuagui (the
        // same name never appears in two groups), so `group` is intentionally
        // not part of the file name and name collisions are not a concern.
        let zip_path = self
            .book_dir
            .join(format!("{}_{}.cbz", self.book_safe, chap_safe));
        if zip_path.exists() {
            println!("{} already exists, skipping.", zip_path.display());
            return Ok(false);
        }
        let chapter_url = resolve_url(href)?;
        let chap = self.get_chapter(chapter_url.as_str())?;
        let chapter_dir = self.book_dir.join(&chap_safe);
        fs::create_dir_all(&chapter_dir)?;
        let bar = ProgressBar::new(chap.files.len() as u64);
        bar.set_style(BAR_STYLE.clone());
        bar.set_message(name.clone());

        match self
            .download_images(&chap, &chapter_dir, &bar, chapter_url.as_str())
            .and_then(|names| Self::compress_chapter(&chapter_dir, &names, &zip_path))
        {
            Ok(()) => {
                bar.finish();
                Ok(true)
            }
            Err(e) => {
                // Release the bar's draw state so the caller's error message
                // prints on its own line instead of over the unfinished bar.
                bar.abandon();
                Err(e)
            }
        }
    }
}

/// Print `prompt`, then read one line and return it trimmed.
/// Errors with `UnexpectedEof` if the input stream is closed.
fn prompt_line<R: io::BufRead>(reader: &mut R, prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    if reader.read_line(&mut input)? == 0 {
        return Err(AppError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stdin closed while waiting for input",
        )));
    }
    Ok(input.trim().to_string())
}

/// Prompt until `parse` accepts a line. A rejected line prints `error` and
/// re-prompts; only a closed input stream ends the loop, as an error.
fn prompt_until_valid<R: io::BufRead, T>(
    reader: &mut R,
    prompt: &str,
    error: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T> {
    loop {
        let input = prompt_line(reader, prompt)?;
        if let Some(value) = parse(&input) {
            return Ok(value);
        }
        eprintln!("{error}");
    }
}

fn prompt_for_comic_selection<R: io::BufRead>(reader: &mut R, comics_count: usize) -> Result<usize> {
    prompt_until_valid(
        reader,
        "Select a comic (enter number): ",
        &format!(
            "Invalid selection. Please enter a number between 1 and {}.",
            comics_count
        ),
        |input| match input.parse::<usize>() {
            Ok(n) if (1..=comics_count).contains(&n) => Some(n - 1),
            _ => None,
        },
    )
}

/// Parse a 1-based chapter selection like "1-3,5" into sorted, deduped
/// 0-based indices. Each range's bounds are validated before it is expanded,
/// so a typo like "1-999999999" is rejected up front instead of allocating
/// billions of entries. Returns `None` on any syntax or bounds error; the
/// whole input is rejected rather than silently dropping the bad part.
fn parse_chapter_selection(input: &str, chapters_count: usize) -> Option<Vec<usize>> {
    let mut indices: Vec<usize> = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse::<usize>().ok()?),
            None => {
                let n = part.parse().ok()?;
                (n, n)
            }
        };
        if start == 0 || start > end || end > chapters_count {
            return None;
        }
        indices.extend(start - 1..end);
    }
    indices.sort_unstable();
    indices.dedup();
    Some(indices)
}

fn prompt_for_chapters<R: io::BufRead>(reader: &mut R, chapters_count: usize) -> Result<Vec<usize>> {
    prompt_until_valid(
        reader,
        "Select chapters (e.g. 1-3,5): ",
        &format!(
            "Invalid selection. Please enter numbers between 1 and {} (e.g. 1-3,5).",
            chapters_count
        ),
        |input| parse_chapter_selection(input, chapters_count),
    )
}

/// Search for `keyword`, page through the results interactively, and let the
/// user pick a comic. Returns the selected comic's ID.
fn interactive_search<R: io::BufRead>(
    client: &Client,
    reader: &mut R,
    keyword: &str,
) -> Result<usize> {
    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut referer = HOST_URL.clone();
    let mut next_url = Some(resolve_url(&format!(
        "/s/{}.html",
        urlencoding::encode(keyword)
    ))?);

    println!("Search results for '{}':", keyword);

    while let Some(url) = next_url {
        let (page_results, maybe_next) =
            parse_search_results(&fetch_html(client, url.as_str(), referer.as_str())?);
        referer = url;
        let offset = all_results.len();
        for (i, r) in page_results.iter().enumerate() {
            println!("{}. {}", offset + i + 1, r.title);
        }
        all_results.extend(page_results);

        next_url = match maybe_next {
            Some(href) => {
                print!("--- Press SPACE for next page, any other key to stop ---");
                io::stdout().flush()?;
                let advance = wait_for_space();
                println!();
                if advance? {
                    Some(resolve_url(&href)?)
                } else {
                    None
                }
            }
            None => None,
        };
    }

    if all_results.is_empty() {
        return Err(AppError::NoSearchResults(keyword.to_string()));
    }

    let selected = prompt_for_comic_selection(reader, all_results.len())?;
    Ok(all_results[selected].comic_id)
}

/// `main` deliberately does not return `Result`: the `Termination` impl for
/// `Result<T, E>` reports the error with `Debug`, which would print
/// `NoSearchResults("金田一")` instead of the `#[error(...)]` text every
/// `AppError` variant carries. Report `Display` here and hand back a plain
/// exit code — this is the only place a user-facing error is printed.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let client = build_client()?;
    let mut stdin = io::stdin().lock();

    let id = if let Some(ref search_keyword) = args.search {
        interactive_search(&client, &mut stdin, search_keyword)?
    } else {
        let url = args
            .url
            .as_deref()
            .expect("clap's required_unless_present guarantees url when search is absent");
        parse_id(url).ok_or(AppError::InvalidUrl)?
    };

    let comic = Comic::new(id, client, &args)?;
    println!("Title: {}", comic.title);
    let mut last_group = "";
    for (i, chapter) in comic.chapters.iter().enumerate() {
        if chapter.group != last_group {
            println!("{}:", chapter.group);
            last_group = &chapter.group;
        }
        println!("  {}: {}", i + 1, chapter.name);
    }

    let indices = prompt_for_chapters(&mut stdin, comic.chapters.len())?;

    for (k, &idx) in indices.iter().enumerate() {
        // Whether to pause before the next chapter: after an actual download,
        // or after an error (to avoid hammering the server on a connection
        // issue). A skipped, already-present chapter needs no pause.
        let should_pause = match comic.download_chapter(idx) {
            Ok(downloaded) => downloaded,
            Err(e) => {
                eprintln!("Failed to download chapter {}: {}", idx + 1, e);
                true
            }
        };
        if should_pause && k + 1 < indices.len() {
            thread::sleep(Duration::from_secs(5));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
