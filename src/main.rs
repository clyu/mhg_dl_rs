use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, InvalidHeaderValue},
};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json;
use lz_string;
use regex::Regex;
use rand::Rng;
use std::{
    fs,
    io::{self, Write},
    num::ParseIntError,
    path::PathBuf,
    thread,
    time::Duration,
};
use thiserror::Error;
use zip::{result::ZipError, write::FileOptions, CompressionMethod, ZipWriter};
use once_cell::sync::Lazy;

static RE_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:https?://(?:[\w\.]+\.)?manhuagui\.com/comic/)?(\d+)").unwrap());
static RE_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\w+\b").unwrap());
static RE_JSON: Lazy<Regex> = Lazy::new(|| Regex::new(r".*\((\{.*\})\).*").unwrap());
static RE_CHAPTER_DATA: Lazy<Regex> = Lazy::new(|| Regex::new(r".*}\('\s*(.*?)',(\d+),(\d+),'([\w+/=]+)'.*").unwrap());
static RE_ILLEGAL_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r##"[\/:*?"<>|]"##).unwrap());


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
    url: String,
    /// Tunnel line: 0=i,1=eu,2=us
    #[clap(short, long, default_value_t = 0)]
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

#[derive(Deserialize)]
struct ChapterStruct {
    sl: Sl,
    path: String,
    files: Vec<String>,
}

#[derive(Deserialize)]
struct Sl {
    e: serde_json::Value,
    m: String,
}

struct Comic {
    client: Client,
    host: String,
    tunnel: String,
    delay: Duration,
    title: String,
    chapters: Vec<(String, String)>,
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
        if base > DIGITS.len() {
            return Err(AppError::ContentParsing(format!(
                "Base {} exceeds supported alphabet size (max {})",
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
            res.insert(0, DIGITS.chars().nth(rem).unwrap());
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
    for i in (0..c).rev() {
        let key = encode(i, a)?;
        let val = if data[i].is_empty() {
            key.clone()
        } else {
            data[i].clone()
        };
        dmap.insert(key, val);
    }
    // replace encoded tokens (words) with their mapped values to reconstruct JS source
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

impl Comic {
    fn new(id: usize, tunnel: usize, delay_ms: u64, output_dir: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        for (key, value) in &[
            ("accept", "image/webp,image/apng,image/*,*/*;q=0.8"),
            ("accept-encoding", "gzip, deflate, br"),
            ("accept-language", "zh-TW,zh;q=0.9,en-US;q=0.8,en;q=0.7,zh-CN;q=0.6"),
            ("cache-control", "no-cache"),
            ("pragma", "no-cache"),
            ("referer", "https://www.manhuagui.com/"),
            ("sec-fetch-dest", "image"),
            ("sec-fetch-mode", "no-cors"),
            ("sec-fetch-site", "cross-site"),
            ("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/81.0.4044.129 Safari/537.36"),
        ] {
            headers.insert(*key, value.parse()?);
        }
        let client = Client::builder().default_headers(headers).build()?;
        let host = String::from("https://tw.manhuagui.com");
        let channels = ["i", "eu", "us"];
        let tn = channels.get(tunnel).unwrap_or(&"i");
        let tunnel_url = format!("https://{}.hamreus.com", tn);
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
        let document = Html::parse_document(&res);
        let sel_title = Selector::parse(".book-title h1")
            .map_err(|e| AppError::ContentParsing(format!("Failed to parse title selector: {:?}", e)))?;
        self.title = document
            .select(&sel_title)
            .next()
            .map(|e| e.text().collect::<String>())
            .ok_or_else(|| AppError::ContentParsing(format!("Could not find title for comic {}", id)))?;
        let sel_chap = Selector::parse(".chapter-list ul a")
            .map_err(|e| AppError::ContentParsing(format!("Failed to parse chapter selector: {:?}", e)))?;
        let elements: Vec<_> = document.select(&sel_chap).collect();
        for element in elements.into_iter().rev() {
            let name = element
                .value()
                .attr("title")
                .ok_or_else(|| AppError::ContentParsing("Chapter title attribute not found".to_string()))?
                .to_string();
            let href = element
                .value()
                .attr("href")
                .ok_or_else(|| AppError::ContentParsing("Chapter href attribute not found".to_string()))?
                .to_string();
            self.chapters.push((name, href));
        }
        Ok(())
    }

    fn get_chapter(&self, href: &str) -> Result<ChapterStruct> {
        let url = format!("{}{}", self.host, href);
        let text = self.client.get(&url).send()?.text()?;
        let caps = RE_CHAPTER_DATA
            .captures(&text)
            .ok_or_else(|| AppError::ContentParsing(format!("Could not parse chapter data from {}", url)))?;

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

    fn download_images(&self, chap: &ChapterStruct, chapter_dir: &PathBuf, bar: &ProgressBar) -> Result<()> {
        let width = (chap.files.len() as f64).log10().floor() as usize + 1;
        for (i, file) in chap.files.iter().enumerate() {
            let url = format!("{}{}{}", self.tunnel, chap.path, file);
            let dst = chapter_dir.join(format!("{:0width$}_{}", i, file, width = width));
            let dst_part = PathBuf::from(format!("{}.part", dst.display()));
            if dst.exists() {
                bar.inc(1);
                continue;
            }
            let e_str = match &chap.sl.e {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => return Err(AppError::ContentParsing("sl.e is not a string or number".to_string())),
            };
            let mut resp = self
                .client
                .get(&url)
                .query(&[("e", &e_str), ("m", &chap.sl.m)])
                .send()?;

            if !resp.status().is_success() {
                return Err(AppError::ContentParsing(format!(
                    "Failed to download image: HTTP {} for {}",
                    resp.status(),
                    url
                )));
            }

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

    fn compress_chapter(&self, chapter_dir: &PathBuf, zip_path: &PathBuf) -> Result<()> {
        let zip_file = fs::File::create(&zip_path)?;
        let mut zip = ZipWriter::new(zip_file);
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);

        let mut files: Vec<PathBuf> = fs::read_dir(&chapter_dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();

        // Sort files numerically by the index prefix (e.g., "10" comes after "9")
        files.sort_by_key(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.split('_').next())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(std::usize::MAX)
        });

        for path in files {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                zip.start_file(name, options)?;
                let mut file = fs::File::open(&path)?;
                io::copy(&mut file, &mut zip)?;
            }
        }

        zip.finish()?;
        fs::remove_dir_all(&chapter_dir)?;
        Ok(())
    }

    fn download_chapter(&self, index: usize) -> Result<()> {
        let (ref name, ref href) = self.chapters[index];
        let book_safe = RE_ILLEGAL_CHARS.replace_all(&self.title, "_");
        let chap_safe = RE_ILLEGAL_CHARS.replace_all(&name, "_");
        let book_dir = PathBuf::from(&self.output_dir).join(book_safe.as_ref());
        let zip_path = book_dir.join(format!("{}.zip", chap_safe));
        if zip_path.exists() {
            println!("{} already exists, skipping.", zip_path.display());
            return Ok(());
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
                .unwrap() // This unwrap is on ProgressStyle, which is safe if the template is valid
                .progress_chars("#>-"),
        );
        bar.set_message(name.clone());

        self.download_images(&chap, &chapter_dir, &bar)?;
        self.compress_chapter(&chapter_dir, &zip_path)?;

        bar.finish();
        Ok(())
    }
}

fn prompt_for_chapters(chapters_count: usize) -> Result<impl Iterator<Item = usize>> {
    loop {
        print!("Select chapters (e.g. 1-3,5): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match range_parser::parse(input.trim()) {
            Ok(parsed_ranges) => {
                // The user enters 1-based chapter numbers. We convert them to 0-based indices.
                let mut indices: Vec<usize> = parsed_ranges
                    .into_iter()
                    .filter_map(|n: u32| n.checked_sub(1)) // Safely subtract 1, filtering out 0
                    .map(|n| n as usize) // Convert u32 to usize
                    .collect();
                indices.sort();
                indices.dedup();

                if let Some(&last_index) = indices.last() {
                    if last_index >= chapters_count {
                        eprintln!(
                            "Error: Chapter {} is out of bounds. Total chapters: {}. Please enter again.",
                            last_index + 1,
                            chapters_count
                        );
                        continue;
                    }
                }

                // If the input was not empty but the list of indices is, it means the user
                // might have entered only "0" or other invalid ranges that the parser handled gracefully.
                if indices.is_empty() && !input.trim().is_empty() {
                    eprintln!("Invalid chapter selection. Please enter numbers between 1 and {}.", chapters_count);
                    continue;
                }

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
    let id = parse_id(&args.url).ok_or(AppError::InvalidUrl)?;
    let comic = Comic::new(
        id,
        args.tunnel,
        args.delay_ms,
        args.output_dir,
    )?;
    println!("Title: {}", comic.title);
    for (i, (name, _)) in comic.chapters.iter().enumerate() {
        println!("{}: {}", i + 1, name);
    }

    let mut ranges = prompt_for_chapters(comic.chapters.len())?.peekable();

    while let Some(idx) = ranges.next() {
        if let Err(e) = comic.download_chapter(idx) {
            eprintln!("Failed to download chapter {}: {}", idx + 1, e);
        }
        if ranges.peek().is_some() {
            thread::sleep(Duration::from_secs(5));
        }
    }
    Ok(())
}
