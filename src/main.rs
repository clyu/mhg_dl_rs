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
    Regex::new(r"^(?:https?://(?:[\w\.]+\.)?manhuagui\.com/comic/)?(\d+)").unwrap()
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
static SEL_H4_SCOPED: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".chapter h4").unwrap());
static SEL_H4_BARE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h4").unwrap());
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
    #[error("Chapter selection parsing error: {0}")]
    RangeParse(#[from] range_parser::RangeError),
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
    e: serde_json::Value,
    m: String,
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
    output_dir: String,
}

fn unpack_packed(
    frame: &str,
    a: usize,
    c: usize,
    data: Vec<String>,
) -> Result<ChapterStruct> {
    fn encode(mut value: usize, base: usize) -> Result<String> {
        const DIGITS: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        if base < 2 || base > DIGITS.len() {
            return Err(AppError::ContentParsing(format!(
                "Base {} out of supported range (2..={})",
                base,
                DIGITS.len()
            )));
        }
        if value == 0 {
            return Ok("0".to_string());
        }
        let mut res = String::new();
        while value > 0 {
            let rem = value % base;
            res.insert(0, DIGITS.as_bytes()[rem] as char);
            value /= base;
        }
        Ok(res)
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
        let key = encode(i, a)?;
        let val = if data[i].is_empty() {
            key.clone()
        } else {
            data[i].clone()
        };
        dmap.insert(key, val);
    }
    let js = RE_WORD
        .replace_all(frame, |caps: &regex::Captures| {
            let key = caps.get(0).unwrap().as_str();
            dmap.get(key).cloned().unwrap_or_else(|| key.to_string())
        })
        .to_string();
    let caps = RE_JSON.captures(&js).ok_or_else(|| {
        AppError::ContentParsing("Could not find JSON data in unpacked script.".to_string())
    })?;
    let json_str = caps
        .get(1)
        .ok_or_else(|| {
            AppError::ContentParsing(
                "Could not extract JSON string from unpacked script.".to_string(),
            )
        })?
        .as_str();
    Ok(serde_json::from_str(json_str)?)
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

fn search_comics(client: &Client, url: &str) -> Result<(Vec<SearchResult>, Option<String>)> {
    let res = client.get(url).send()?.text()?;
    parse_search_results(&res)
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

fn parse_search_results(html: &str) -> Result<(Vec<SearchResult>, Option<String>)> {
    let document = Html::parse_document(html);

    let mut results = Vec::new();

    for li in document.select(&SEL_COMICS) {
        if let Some(link) = li.select(&SEL_LINK).next() {
            if let Some(href) = link.value().attr("href") {
                if let Some(id_str) = href.split('/').find(|s| !s.is_empty() && s.chars().all(|c| c.is_numeric())) {
                    if let Ok(comic_id) = id_str.parse::<usize>() {
                        if let Some(title) = link.value().attr("title") {
                            results.push(SearchResult {
                                title: title.to_string(),
                                comic_id,
                            });
                        }
                    }
                }
            }
        }
    }

    let next_page = document.select(&SEL_PAGER_LINKS)
        .find(|a| a.text().collect::<String>().trim() == "下一頁")
        .and_then(|a| a.value().attr("href"))
        .map(|s| s.to_string());

    Ok((results, next_page))
}

fn chapters_from_elements_with_tag<'a>(
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

fn extract_chapters_with_groups(document: &Html) -> Result<Vec<Chapter>> {
    let headers: Vec<String> = {
        let scoped: Vec<_> = document.select(&SEL_H4_SCOPED).collect();
        let sel = if scoped.is_empty() { &SEL_H4_BARE } else { &SEL_H4_SCOPED };
        document
            .select(sel)
            .map(|h| h.text().collect::<String>().trim().to_string())
            .collect()
    };

    let lists: Vec<scraper::ElementRef> = document.select(&SEL_CHAPTER_LIST).collect();

    let mut chapters = Vec::new();

    if !lists.is_empty() {
        let mut headers = headers;
        while headers.len() < lists.len() {
            headers.push("Chapters".to_string());
        }

        for (i, list_elem) in lists.iter().enumerate() {
            let tag = &headers[i];
            for ul_elem in list_elem.select(&SEL_UL) {
                let ul_chaps = chapters_from_elements_with_tag(ul_elem.select(&SEL_CHAP_INNER), tag)?;
                chapters.extend(ul_chaps);
            }
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
        let tunnel_url = format!("https://{}.hamreus.com", TUNNEL_CHANNELS[tunnel]);
        let mut c = Comic {
            client,
            host,
            tunnel: tunnel_url,
            delay: Duration::from_millis(delay_ms),
            title: String::new(),
            chapters: Vec::new(),
            output_dir,
        };
        c.load_metadata(id)?;
        Ok(c)
    }

    fn load_metadata(&mut self, id: usize) -> Result<()> {
        let url = format!("{}/comic/{}", self.host, id);
        let res = self.client.get(&url).send()?.text()?;
        let (title, chapters) = Self::parse_comic_html(&res)?;
        self.title = title;
        self.chapters = chapters;
        Ok(())
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
                let decoded = lz_string::Decoder::new()
                    .decode_base64(vs_val)
                    .map_err(|_| AppError::ContentParsing(
                        "Failed to decode __VIEWSTATE".to_string(),
                    ))?;
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

    fn get_chapter(&self, href: &str) -> Result<ChapterStruct> {
        let url = format!("{}{}", self.host, href);
        let text = self.client.get(&url).send()?.text()?;
        Self::parse_chapter_html(&text)
    }

    fn parse_chapter_html(html: &str) -> Result<ChapterStruct> {
        let caps = RE_CHAPTER_DATA
            .captures(html)
            .ok_or_else(|| AppError::ContentParsing("Could not parse chapter data".to_string()))?;

        let get_cap = |i: usize| {
            caps.get(i)
                .map(|m| m.as_str())
                .ok_or_else(|| AppError::ContentParsing(format!("Capture group {} not found", i)))
        };

        let frame = get_cap(1)?;
        let a: usize = get_cap(2)?.parse()?;
        let c: usize = get_cap(3)?.parse()?;
        let data_b64 = get_cap(4)?;

        let data_dec = lz_string::Decoder::new()
            .decode_base64(data_b64)
            .map_err(|_| AppError::ContentParsing("Failed to decode base64 chapter data".to_string()))?;
        let data = data_dec.split('|').map(|s| s.to_string()).collect();
        unpack_packed(frame, a, c, data)
    }

    fn download_images(&self, chap: &ChapterStruct, chapter_dir: &Path, bar: &ProgressBar, chapter_url: &str) -> Result<()> {
        let width = chap.files.len().saturating_sub(1).to_string().len();
        let e_str = match &chap.sl.e {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => return Err(AppError::ContentParsing("sl.e is not a string or number".to_string())),
        };
        for (i, file) in chap.files.iter().enumerate() {
            let url = format!("{}{}{}", self.tunnel, chap.path, file);
            let file_safe = RE_ILLEGAL_CHARS.replace_all(file, "_");
            let fname = format!("{:0width$}_{}", i, file_safe, width = width);
            let dst = chapter_dir.join(&fname);
            let dst_part = chapter_dir.join(format!("{fname}.part"));

            if dst.exists() {
                bar.inc(1);
                continue;
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
            thread::sleep(rand::rng().random_range(self.delay / 2..=self.delay * 3 / 2));
        }
        Ok(())
    }

    fn compress_chapter(chapter_dir: &Path, zip_path: &Path) -> Result<()> {
        let mut zip_part = zip_path.as_os_str().to_owned();
        zip_part.push(".part");
        let zip_part = PathBuf::from(zip_part);
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
        let book_safe = RE_ILLEGAL_CHARS.replace_all(&self.title, "_");
        let chap_safe = RE_ILLEGAL_CHARS.replace_all(name, "_");
        let book_dir = PathBuf::from(&self.output_dir).join(book_safe.as_ref());
        let zip_path = book_dir.join(format!("{}_{}.cbz", book_safe, chap_safe));
        if zip_path.exists() {
            println!("{} already exists, skipping.", zip_path.display());
            return Ok(false);
        }
        let chap = self.get_chapter(href)?;
        let chapter_dir = book_dir.join(chap_safe.as_ref());
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

        let chapter_url = format!("{}{}", self.host, href);
        self.download_images(&chap, &chapter_dir, &bar, &chapter_url)?;
        Comic::compress_chapter(&chapter_dir, &zip_path)?;

        bar.finish();
        Ok(true)
    }
}

fn prompt_for_comic_selection<R: io::BufRead>(reader: &mut R, comics_count: usize) -> Result<usize> {
    loop {
        print!("Select a comic (enter number): ");
        io::stdout().flush()?;
        let mut input = String::new();
        if reader.read_line(&mut input)? == 0 {
            return Err(AppError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdin closed while waiting for comic selection",
            )));
        }
        match input.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= comics_count => {
                return Ok(n - 1);
            }
            _ => {
                eprintln!("Invalid selection. Please enter a number between 1 and {}.", comics_count);
                continue;
            }
        }
    }
}

fn prompt_for_chapters<R: io::BufRead>(reader: &mut R, chapters_count: usize) -> Result<impl Iterator<Item = usize>> {
    loop {
        print!("Select chapters (e.g. 1-3,5): ");
        io::stdout().flush()?;
        let mut input = String::new();
        if reader.read_line(&mut input)? == 0 {
            return Err(AppError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdin closed while waiting for chapter selection",
            )));
        }
        match range_parser::parse(input.trim()) {
            Ok(parsed_ranges) => {
                // The user enters 1-based chapter numbers; reject the whole input if
                // any of them is out of range instead of silently dropping it.
                if parsed_ranges.is_empty()
                    || parsed_ranges
                        .iter()
                        .any(|&n: &u32| n == 0 || n as usize > chapters_count)
                {
                    eprintln!("Invalid chapter selection. Please enter numbers between 1 and {}.", chapters_count);
                    continue;
                }

                // Convert to 0-based indices.
                let mut indices: Vec<usize> = parsed_ranges
                    .into_iter()
                    .map(|n| n as usize - 1)
                    .collect();
                indices.sort();
                indices.dedup();

                return Ok(indices.into_iter());
            }
            Err(_) => {
                eprintln!("Invalid input format. Please enter again.");
                continue;
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let client = build_client()?;

    let id = if let Some(ref search_keyword) = args.search {
        let first_url = format!("{}/s/{}.html", HOST, urlencoding::encode(search_keyword));
        let mut all_results: Vec<SearchResult> = Vec::new();
        let mut next_url: Option<String> = Some(first_url);

        println!("Search results for '{}':", search_keyword);

        while let Some(url) = next_url {
            let (page_results, maybe_next) = search_comics(&client, &url)?;
            let offset = all_results.len();
            for (i, r) in page_results.iter().enumerate() {
                println!("{}. {}", offset + i + 1, r.title);
            }
            all_results.extend(page_results);

            next_url = if let Some(path) = maybe_next {
                print!("--- Press SPACE for next page, any other key to stop ---");
                io::stdout().flush()?;
                if wait_for_space() {
                    println!();
                    Some(format!("{}{}", HOST, path))
                } else {
                    println!();
                    None
                }
            } else {
                None
            };
        }

        if all_results.is_empty() {
            eprintln!("No comics found for '{}'", search_keyword);
            return Err(AppError::ContentParsing("No search results".to_string()));
        }

        let mut stdin = io::stdin().lock();
        let selected_id = prompt_for_comic_selection(&mut stdin, all_results.len())?;
        drop(stdin);
        all_results[selected_id].comic_id
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
    let mut last_group = String::new();
    for (i, chapter) in comic.chapters.iter().enumerate() {
        if chapter.group != last_group {
            println!("{}:", chapter.group);
            last_group = chapter.group.clone();
        }
        println!("  {}: {}", i + 1, chapter.name);
    }

    let mut stdin = io::stdin().lock();
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
