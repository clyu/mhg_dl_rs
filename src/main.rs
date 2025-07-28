use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{blocking::Client, header::HeaderMap};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json;
use lz_string;
use regex::Regex;
use rand::Rng;
use std::{fs, io::{self, Write}, path::Path, thread, time::Duration};
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

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
    /// Skip existing images
    #[clap(short, long, default_value_t = true)]
    skip: bool,
    /// Output directory
    #[clap(short, long, default_value = "Downloads")]
    output_dir: String,
}

fn parse_id(s: &str) -> Option<usize> {
    let re = Regex::new(r"^(?:https?://(?:[\w\.]+\.)?manhuagui\.com/comic/)?(\d+)").unwrap();
    re.captures(s)
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
struct Sl { e: serde_json::Value, m: String }

struct Comic {
    client: Client,
    host: String,
    tunnel: String,
    delay: Duration,
    skip: bool,
    title: String,
    chapters: Vec<(String, String)>,
    output_dir: String,
}

impl Comic {
    fn new(id: usize, tunnel: usize, delay_ms: u64, skip: bool, output_dir: String) -> io::Result<Self> {
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
            headers.insert(*key, value.parse().unwrap());
        }
        let client = Client::builder().default_headers(headers).build().unwrap();
        let host = String::from("https://tw.manhuagui.com");
        let channels = ["i", "eu", "us"];
        let tn = channels.get(tunnel).unwrap_or(&"i");
        let tunnel_url = format!("https://{}{}.hamreus.com", tn, "");
        let mut c = Comic {
            client,
            host,
            tunnel: tunnel_url,
            delay: Duration::from_millis(delay_ms),
            skip,
            title: String::new(),
            chapters: Vec::new(),
            output_dir,
        };
        c.load_metadata(id)?;
        Ok(c)
    }

    fn load_metadata(&mut self, id: usize) -> io::Result<()> {
        let url = format!("{}/comic/{}", self.host, id);
        let res = self.client.get(&url).send().unwrap().text().unwrap();
        let document = Html::parse_document(&res);
        let sel_title = Selector::parse(".book-title h1").unwrap();
        self.title = document
            .select(&sel_title)
            .next()
            .map(|e| e.text().collect())
            .unwrap_or_else(|| id.to_string());
        let sel_chap = Selector::parse(".chapter-list ul a").unwrap();
        let elements: Vec<_> = document.select(&sel_chap).collect();
        for element in elements.into_iter().rev() {
            let name = element.value().attr("title").unwrap_or("").to_string();
            let href = element.value().attr("href").unwrap_or("").to_string();
            self.chapters.push((name, href));
        }
        Ok(())
    }

    fn unpack_packed(&self, frame: &str, a: usize, c: usize, data: Vec<String>) -> ChapterStruct {
        fn convert_base(mut value: usize, base: usize) -> String {
            let digits = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
            if value == 0 { return "0".to_string(); }
            let mut res = String::new();
            while value > 0 {
                let rem = value % base;
                res.insert(0, digits.chars().nth(rem).unwrap());
                value /= base;
            }
            res
        }
        fn encode(inner: usize, a: usize) -> String {
            if inner < a {
                if inner > 35 {
                    (((inner % a) as u8 + 29) as char).to_string()
                } else {
                    convert_base(inner, 36)
                }
            } else {
                let rec = encode(inner / a, a);
                let ch = if inner % a > 35 {
                    ((inner % a) as u8 + 29) as char
                } else {
                    convert_base(inner % a, 36).chars().next().unwrap()
                };
                format!("{}{}", rec, ch)
            }
        }
        let mut dmap = std::collections::HashMap::new();
        for i in (0..c).rev() {
            let key = encode(i, a);
            let val = if data[i].is_empty() { key.clone() } else { data[i].clone() };
            dmap.insert(key, val);
        }
        // replace encoded tokens (words) with their mapped values to reconstruct JS source
        let re_word = Regex::new(r"\b\w+\b").unwrap();
        let js = re_word.replace_all(frame, |caps: &regex::Captures| {
            let key = caps.get(0).unwrap().as_str();
            dmap.get(key).cloned().unwrap_or_else(|| key.to_string())
        }).to_string();
        let re_json = Regex::new(r".*\((\{.*\})\).*").unwrap();
        let caps = re_json.captures(&js).unwrap();
        let json_str = caps.get(1).unwrap().as_str();
        serde_json::from_str(json_str).unwrap()
    }

    fn get_chapter(&self, href: &str) -> ChapterStruct {
        let url = format!("{}{}", self.host, href);
        let text = self.client.get(&url).send().unwrap().text().unwrap();
        let re = Regex::new(r".*}\('\s*(.*?)',(\d+),(\d+),'([\w+/=]+)'.*").unwrap();
        let caps = re.captures(&text).unwrap();
        let frame = caps.get(1).unwrap().as_str();
        let a: usize = caps.get(2).unwrap().as_str().parse().unwrap();
        let c: usize = caps.get(3).unwrap().as_str().parse().unwrap();
        let data_b64 = caps.get(4).unwrap().as_str();
        let data_dec = lz_string::Decoder::new().decode_base64(data_b64).unwrap();
        let data = data_dec.split('|').map(|s| s.to_string()).collect();
        self.unpack_packed(frame, a, c, data)
    }

    fn download_chapter(&self, index: usize) {
        let (ref name, ref href) = self.chapters[index];
        let illegal = Regex::new(r##"[\/:*?"<>|]"##).unwrap();
        let book_safe = illegal.replace_all(&self.title, "_");
        let chap_safe = illegal.replace_all(&name, "_");
        let zip_path = format!("{}/{}/{}.zip", self.output_dir, book_safe, chap_safe);
        if self.skip && Path::new(&zip_path).exists() {
            println!("{} already exists, skipping.", &zip_path);
            return;
        }
        let chap = self.get_chapter(href);
        let folder = format!("{}/{}/{}", self.output_dir, book_safe, chap_safe);
        fs::create_dir_all(&folder).unwrap();
        let bar = ProgressBar::new(chap.files.len() as u64);
        bar.set_style(ProgressStyle::default_bar().template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
            ).unwrap().progress_chars("#>-"),);
        bar.set_message(name.clone());
        for (i, file) in chap.files.iter().enumerate() {
            let url = format!("{}{}{}", self.tunnel, chap.path, file);
            let dst = format!("{}/{}_{}", folder, i, file);
            let dst_part = format!("{}.part", dst);
            if self.skip && Path::new(&dst).exists() {
                bar.inc(1);
                continue;
            }
            let e_str = match &chap.sl.e {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => panic!("sl.e is not a string or number"),
            };
            let mut resp = self.client.get(&url)
                .query(&[("e", &e_str), ("m", &chap.sl.m)])
                .send().unwrap();
            let mut out = fs::File::create(&dst_part).unwrap();
            io::copy(&mut resp, &mut out).unwrap();
            fs::rename(&dst_part, &dst).unwrap();
            thread::sleep(rand::rng().random_range(self.delay/2..=self.delay*3/2));
            bar.inc(1);
        }
        let zip_file = fs::File::create(&zip_path).unwrap();
        let mut zip = ZipWriter::new(zip_file);
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);
        for entry in fs::read_dir(&folder).unwrap() {
            let e = entry.unwrap();
            let path = e.path();
            if path.is_file() {
                let name = path.file_name().unwrap().to_string_lossy();
                zip.start_file(name, options).unwrap();
                let data = fs::read(&path).unwrap();
                zip.write_all(&data).unwrap();
                fs::remove_file(&path).unwrap();
            }
        }
        zip.finish().unwrap();
        fs::remove_dir(&folder).unwrap();
    }
}

fn main() {
    let args = Args::parse();
    let id = parse_id(&args.url).expect("Invalid manhuagui URL or ID");
    let comic = Comic::new(id, args.tunnel, args.delay_ms, args.skip, args.output_dir).expect("Failed to init comic");
    println!("Title: {}", comic.title);
    for (i, (name, _)) in comic.chapters.iter().enumerate() {
        println!("{}: {}", i, name);
    }
    print!("Select chapters (e.g. 1-3,5): "); io::stdout().flush().unwrap();
    let mut input = String::new(); io::stdin().read_line(&mut input).unwrap();
    let mut ranges = range_parser::parse(&input).unwrap_or_default().into_iter().peekable();
    while let Some(idx) = ranges.next() {
        comic.download_chapter(idx);
        if ranges.peek().is_some() {
            thread::sleep(Duration::from_secs(5));
        }
    }
}
