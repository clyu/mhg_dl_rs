use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    terminal,
};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, InvalidHeaderValue},
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
    thread,
    time::Duration,
};
use thiserror::Error;
use zip::{result::ZipError, write::FileOptions, CompressionMethod, ZipWriter};
use std::sync::LazyLock;

const HOST: &str = "https://tw.manhuagui.com";
const TUNNEL_CHANNELS: [&str; 3] = ["i", "eu", "us"];

static RE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https?://(?:[\w\.]+\.)?manhuagui\.com/comic/)?(\d+)\b").unwrap()
});
static RE_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\w+\b").unwrap());
static RE_JSON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\{.*?\})\)").unwrap());
static RE_CHAPTER_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"}\('\s*(.*?)',(\d+),(\d+),'([\w+/=]+)'").unwrap());
static RE_ILLEGAL_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r##"[\\/:*?"<>|]"##).unwrap());

static SEL_COMICS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.book-result ul li.cf").unwrap());
static SEL_LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.bcover").unwrap());
static SEL_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".book-title h1").unwrap());
static SEL_CHAP_INNER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static SEL_UL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("ul").unwrap());
static SEL_PAGER_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.pager a").unwrap());
static SEL_VIEWSTATE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("input#__VIEWSTATE").unwrap());
static SEL_CHAPTER_LIST: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".chapter-list").unwrap());

#[derive(Error, Debug)]
enum AppError {
    #[error("Invalid manhuagui URL or ID")]
    InvalidUrl,
    #[error("Content parsing error: {0}")]
    ContentParsing(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("Network request error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Invalid HTTP header: {0}")]
    InvalidHeader(#[from] InvalidHeaderValue),
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
    /// Tunnel line: 0=i,1=eu,2=us
    #[clap(short, long, default_value_t = 0, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(..TUNNEL_CHANNELS.len() as u64))]
    tunnel: usize,
    /// Delay between pages in milliseconds
    #[clap(short, long, default_value_t = 1000)]
    delay_ms: u64,
    /// Output directory
    #[clap(short, long, default_value = "Downloads")]
    output_dir: String,
}

fn parse_id(s: &str) -> Option<usize> {
    RE_ID.captures(s)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
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

#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    comic_id: usize,
}

#[derive(Debug, Clone)]
struct Chapter {
    name: String,
    href: String,
    /// Section heading the chapter belongs to, e.g. "單行本" (volumes) or "單話" (single chapters)
    group: String,
}

struct Comic {
    client: Client,
    host: String,
    tunnel: String,
    delay: Duration,
    title: String,
    chapters: Vec<Chapter>,
    /// Sanitized title, used as the book directory name and zip name prefix.
    book_safe: String,
    book_dir: PathBuf,
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
    let mut dmap = std::collections::HashMap::new();
    if c > data.len() {
        return Err(AppError::ContentParsing(format!(
            "Packed script dictionary size mismatch: expected {} words, got {}",
            c,
            data.len()
        )));
    }
    for i in 0..c {
        let key = encode(i, a);
        // An empty dictionary entry maps the word to itself, which is also
        // what the replacement below does for unknown words — skip it.
        if !data[i].is_empty() {
            dmap.insert(key, data[i]);
        }
    }
    let js = RE_WORD
        .replace_all(frame, |caps: &regex::Captures| {
            let key = caps.get(0).unwrap().as_str();
            dmap.get(key).copied().unwrap_or(key).to_string()
        })
        .to_string();
    let caps = RE_JSON.captures(&js).ok_or_else(|| {
        AppError::ContentParsing("Could not find JSON data in unpacked script.".to_string())
    })?;
    Ok(serde_json::from_str(&caps[1])?)
}

/// Replace characters that are invalid in file names with `_`.
fn sanitize(s: &str) -> std::borrow::Cow<'_, str> {
    RE_ILLEGAL_CHARS.replace_all(s, "_")
}

fn decode_lz_base64(data: &str, what: &str) -> Result<String> {
    lz_string::Decoder::new()
        .decode_base64(data)
        .map_err(|_| AppError::ContentParsing(format!("Failed to decode {}", what)))
}

fn build_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    for (key, value) in &[
        ("accept", "image/webp,image/apng,image/*,*/*;q=0.8"),
        ("accept-encoding", "gzip, deflate, br"),
        ("accept-language", "zh-TW,zh;q=0.9,en-US;q=0.8,en;q=0.7,zh-CN;q=0.6"),
        ("cache-control", "no-cache"),
        ("pragma", "no-cache"),
        ("sec-fetch-dest", "document"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-site", "same-origin"),
        ("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/81.0.4044.129 Safari/537.36"),
    ] {
        headers.insert(*key, value.parse()?);
    }
    headers.insert("referer", format!("{}/", HOST).parse()?);
    Ok(Client::builder().default_headers(headers).build()?)
}

fn fetch_html(client: &Client, url: &str) -> Result<String> {
    Ok(client.get(url).send()?.error_for_status()?.text()?)
}

fn wait_for_space() -> bool {
    if terminal::enable_raw_mode().is_err() {
        return false;
    }
    let result = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                kind: KeyEventKind::Press,
                ..
            })) => break true,
            Ok(Event::Key(KeyEvent {
                kind: KeyEventKind::Press,
                ..
            })) => break false,
            Err(_) => break false,
            _ => {}
        }
    };
    let _ = terminal::disable_raw_mode();
    result
}

fn search_result_from_item(li: scraper::ElementRef<'_>) -> Option<SearchResult> {
    let link = li.select(&SEL_LINK).next()?;
    let href = link.value().attr("href")?;
    let comic_id = href
        .split('/')
        .find(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))?
        .parse()
        .ok()?;
    let title = link.value().attr("title")?;
    Some(SearchResult {
        title: title.to_string(),
        comic_id,
    })
}

fn parse_search_results(html: &str) -> Result<(Vec<SearchResult>, Option<String>)> {
    let document = Html::parse_document(html);

    let results: Vec<SearchResult> = document
        .select(&SEL_COMICS)
        .filter_map(search_result_from_item)
        .collect();

    let next_page = document.select(&SEL_PAGER_LINKS)
        .find(|a| a.text().collect::<String>().trim() == "下一頁")
        .and_then(|a| a.value().attr("href"))
        .map(|s| s.to_string());

    Ok((results, next_page))
}

fn chapters_from_elements_with_group<'a>(
    elements: impl Iterator<Item = scraper::ElementRef<'a>>,
    group: &str,
) -> Result<Vec<Chapter>> {
    let mut chapters: Vec<Chapter> = elements
        .map(|element| {
            let attr = |key: &str| {
                element
                    .value()
                    .attr(key)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        AppError::ContentParsing(format!("Chapter {} attribute not found", key))
                    })
            };
            Ok(Chapter {
                name: attr("title")?,
                href: attr("href")?,
                group: group.to_string(),
            })
        })
        .collect::<Result<_>>()?;
    // The site lists chapters newest-first; reverse into reading order
    chapters.reverse();
    Ok(chapters)
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

fn extract_chapters_with_groups(document: &Html) -> Result<Vec<Chapter>> {
    let mut chapters = Vec::new();
    for list_elem in document.select(&SEL_CHAPTER_LIST) {
        let group = group_for_list(list_elem);
        for ul_elem in list_elem.select(&SEL_UL) {
            let ul_chaps =
                chapters_from_elements_with_group(ul_elem.select(&SEL_CHAP_INNER), &group)?;
            chapters.extend(ul_chaps);
        }
    }

    Ok(chapters)
}

impl Comic {
    fn new(
        id: usize,
        client: Client,
        tunnel: usize,
        delay_ms: u64,
        output_dir: String,
    ) -> Result<Self> {
        let host = String::from(HOST);
        let res = fetch_html(&client, &format!("{}/comic/{}", host, id))?;
        let (title, chapters) = Self::parse_comic_html(&res)?;
        let book_safe = sanitize(&title).into_owned();
        let book_dir = PathBuf::from(output_dir).join(&book_safe);
        Ok(Comic {
            client,
            host,
            tunnel: format!("https://{}.hamreus.com", TUNNEL_CHANNELS[tunnel]),
            delay: Duration::from_millis(delay_ms),
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
            .map(|e| e.text().collect::<String>())
            .ok_or_else(|| AppError::ContentParsing("Could not find title".to_string()))?;

        let mut chapters = extract_chapters_with_groups(&document)?;

        if chapters.is_empty() {
            if let Some(vs_val) = document
                .select(&SEL_VIEWSTATE)
                .next()
                .and_then(|e| e.value().attr("value"))
            {
                let decoded = decode_lz_base64(vs_val, "__VIEWSTATE")?;
                let inner = Html::parse_fragment(&decoded);
                chapters = extract_chapters_with_groups(&inner)?;
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
        let text = fetch_html(&self.client, url)?;
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

    fn download_images(&self, chap: &ChapterStruct, chapter_dir: &Path, bar: &ProgressBar, chapter_url: &str) -> Result<()> {
        let width = chap.files.len().saturating_sub(1).to_string().len();
        let e_str = chap.sl.e.to_string();
        let mut needs_delay = false;
        for (i, file) in chap.files.iter().enumerate() {
            let url = format!("{}{}{}", self.tunnel, chap.path, file);
            let file_safe = sanitize(file);
            let fname = format!("{:0width$}_{}", i, file_safe, width = width);
            let dst = chapter_dir.join(&fname);
            let dst_part = chapter_dir.join(format!("{fname}.part"));

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
                .header("referer", chapter_url)
                .header("sec-fetch-dest", "image")
                .header("sec-fetch-mode", "no-cors")
                .header("sec-fetch-site", "cross-site")
                .query(&[("e", &e_str), ("m", &chap.sl.m)])
                .send()?
                .error_for_status()?;

            let content_length = resp.content_length();
            let mut out = fs::File::create(&dst_part)?;
            let bytes_written = io::copy(&mut resp, &mut out)?;

            if let Some(expected) = content_length {
                if bytes_written != expected {
                    return Err(AppError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("Incomplete download: expected {} bytes, got {}", expected, bytes_written),
                    )));
                }
            }

            fs::rename(&dst_part, &dst)?;
            bar.inc(1);
            needs_delay = true;
        }
        Ok(())
    }

    fn compress_chapter(chapter_dir: &Path, zip_path: &Path) -> Result<()> {
        let zip_part = zip_path.with_extension("cbz.part");
        let zip_file = fs::File::create(&zip_part)?;
        let mut zip = ZipWriter::new(zip_file);
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);

        let mut files: Vec<PathBuf> = fs::read_dir(&chapter_dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().map_or(true, |ext| ext != "part"))
            .collect();

        files.sort();

        for path in files {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                zip.start_file(name, options)?;
                let mut file = fs::File::open(&path)?;
                io::copy(&mut file, &mut zip)?;
            }
        }

        zip.finish()?;
        fs::rename(&zip_part, zip_path)?;
        fs::remove_dir_all(&chapter_dir)?;
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
        let chapter_url = format!("{}{}", self.host, href);
        let chap = self.get_chapter(&chapter_url)?;
        let chapter_dir = self.book_dir.join(chap_safe.as_ref());
        fs::create_dir_all(&chapter_dir)?;
        let bar = ProgressBar::new(chap.files.len() as u64);
        bar.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        bar.set_message(name.clone());

        self.download_images(&chap, &chapter_dir, &bar, &chapter_url)?;
        Self::compress_chapter(&chapter_dir, &zip_path)?;

        bar.finish();
        Ok(true)
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

fn prompt_for_comic_selection<R: io::BufRead>(reader: &mut R, comics_count: usize) -> Result<usize> {
    loop {
        let input = prompt_line(reader, "Select a comic (enter number): ")?;
        match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= comics_count => {
                return Ok(n - 1);
            }
            _ => {
                eprintln!("Invalid selection. Please enter a number between 1 and {}.", comics_count);
            }
        }
    }
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

fn prompt_for_chapters<R: io::BufRead>(reader: &mut R, chapters_count: usize) -> Result<impl Iterator<Item = usize>> {
    loop {
        let input = prompt_line(reader, "Select chapters (e.g. 1-3,5): ")?;
        match parse_chapter_selection(&input, chapters_count) {
            Some(indices) => return Ok(indices.into_iter()),
            None => {
                eprintln!(
                    "Invalid selection. Please enter numbers between 1 and {} (e.g. 1-3,5).",
                    chapters_count
                );
            }
        }
    }
}

/// Search for `keyword`, page through the results interactively, and let the
/// user pick a comic. Returns the selected comic's ID.
fn interactive_search<R: io::BufRead>(
    client: &Client,
    reader: &mut R,
    keyword: &str,
) -> Result<usize> {
    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut next_url = Some(format!("{}/s/{}.html", HOST, urlencoding::encode(keyword)));

    println!("Search results for '{}':", keyword);

    while let Some(url) = next_url {
        let (page_results, maybe_next) = parse_search_results(&fetch_html(client, &url)?)?;
        let offset = all_results.len();
        for (i, r) in page_results.iter().enumerate() {
            println!("{}. {}", offset + i + 1, r.title);
        }
        all_results.extend(page_results);

        next_url = if let Some(path) = maybe_next {
            print!("--- Press SPACE for next page, any other key to stop ---");
            io::stdout().flush()?;
            let advance = wait_for_space();
            println!();
            advance.then(|| format!("{}{}", HOST, path))
        } else {
            None
        };
    }

    if all_results.is_empty() {
        eprintln!("No comics found for '{}'", keyword);
        return Err(AppError::ContentParsing("No search results".to_string()));
    }

    let selected = prompt_for_comic_selection(reader, all_results.len())?;
    Ok(all_results[selected].comic_id)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let client = build_client()?;
    let mut stdin = io::stdin().lock();

    let id = if let Some(ref search_keyword) = args.search {
        interactive_search(&client, &mut stdin, search_keyword)?
    } else {
        let url = args.url.as_ref().ok_or(AppError::InvalidUrl)?;
        parse_id(url).ok_or(AppError::InvalidUrl)?
    };

    let comic = Comic::new(
        id,
        client,
        args.tunnel,
        args.delay_ms,
        args.output_dir,
    )?;
    println!("Title: {}", comic.title);
    let mut last_group = "";
    for (i, chapter) in comic.chapters.iter().enumerate() {
        if chapter.group != last_group {
            println!("{}:", chapter.group);
            last_group = &chapter.group;
        }
        println!("  {}: {}", i + 1, chapter.name);
    }

    let mut ranges = prompt_for_chapters(&mut stdin, comic.chapters.len())?.peekable();

    while let Some(idx) = ranges.next() {
        let downloaded = match comic.download_chapter(idx) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to download chapter {}: {}", idx + 1, e);
                true // Sleep on error to avoid rapid retries if there's a connection issue
            }
        };
        if downloaded && ranges.peek().is_some() {
            thread::sleep(Duration::from_secs(5));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
