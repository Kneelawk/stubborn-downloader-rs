#![feature(is_some_with)]

use anyhow::{bail, Context};
use bytes::{Buf, Bytes};
use clap::Parser;
use console::Term;
use futures_util::StreamExt;
use indicatif::{HumanBytes, ProgressBar, ProgressDrawTarget, ProgressStyle};
use lazy_static::lazy_static;
use reqwest::{Client, Url};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, timeout};

lazy_static! {
    static ref PROGRESS_STYLE: ProgressStyle = ProgressStyle::with_template(
        "{percent}% <{bar:20.cyan/blue}> {bytes}/{total_bytes} ({binary_bytes_per_sec}) {wide_msg} {elapsed}/{duration} ({eta} remaining)"
    )
    .unwrap()
    .progress_chars("=+-");
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Url to download from.
    #[clap(value_parser)]
    url: Url,

    /// File to output to.
    #[clap(short, long, value_parser)]
    output: PathBuf,

    /// Number of seconds to wait after every error.
    #[clap(long, value_parser, default_value_t = 10)]
    error_delay: u32,

    /// The amount of time to wait before re-opening a stalled connection.
    #[clap(long, value_parser, default_value_t = 60)]
    connection_timeout: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new();

    let term = Term::stderr();

    let args = Args::parse();

    writeln!(&term, "################").unwrap();
    writeln!(&term, "Download Info:").unwrap();
    writeln!(&term, "URL: {}", &args.url).unwrap();
    writeln!(&term, "Output file: {}", args.output.to_string_lossy()).unwrap();
    writeln!(&term, "################").unwrap();

    {
        let scheme = args.url.scheme();
        if scheme != "http" && scheme != "https" {
            bail!("Download urls must be in either HTTP or HTTPS");
        }
    }

    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args.output)
        .await?;

    let connection_timeout = Duration::from_secs(args.connection_timeout as u64);

    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::term(term.clone(), 8))
        .with_style(ProgressStyle::default_spinner())
        .with_message("Starting download...");

    let should_tick = Arc::new(AtomicBool::new(true));
    let should_tick_2 = should_tick.clone();
    let bar_2 = bar.clone();
    tokio::spawn(async move {
        while should_tick_2.load(Ordering::Relaxed) {
            bar_2.tick();
            sleep(Duration::from_millis(125)).await;
        }
    });

    let mut full_length = None;
    let mut offset = 0u64;
    let mut incomplete = true;

    while incomplete {
        match do_download(
            &client,
            args.url.clone(),
            &mut output,
            connection_timeout,
            term.clone(),
            &bar,
            &mut full_length,
            &mut offset,
            &mut incomplete,
        )
        .await
        {
            Ok(_) => {}
            Err(err) => {
                writeln!(&term, "{}", err).unwrap();
                writeln!(&term, "Download error. Retrying...").unwrap();
            }
        }

        if incomplete && args.error_delay > 0 {
            writeln!(&term, "Sleeping for {} seconds", args.error_delay).unwrap();
            sleep(Duration::from_secs(args.error_delay as u64)).await;
        }
    }

    bar.finish();
    should_tick.store(false, Ordering::Relaxed);
    Ok(())
}

async fn do_download(
    client: &Client,
    url: Url,
    output: &mut File,
    connection_timeout: Duration,
    term: Term,
    bar: &ProgressBar,
    full_length: &mut Option<u64>,
    offset: &mut u64,
    incomplete: &mut bool,
) -> anyhow::Result<()> {
    bar.set_message("Connecting...");
    bar.set_style(ProgressStyle::default_spinner());

    let mut builder = client.get(url);

    if *offset > 0u64 {
        builder = builder.header("range", format!("bytes={}-", offset));
    }

    let res = timeout(connection_timeout, builder.send())
        .await
        .context("Connection timeout")?
        .context("Error connecting to server")?;

    if res.status().is_client_error() || res.status().is_server_error() {
        writeln!(&term, "!!!!!!!!!!!!!!!!").unwrap();
        writeln!(&term, "Server gave bad response code: {}", res.status()).unwrap();
        writeln!(&term, "!!!!!!!!!!!!!!!!").unwrap();
        *incomplete = false;
        return Ok(());
    }

    let length = res.content_length();
    let mut downloaded = 0u64;
    if full_length.is_none() {
        *full_length = length;
    }

    writeln!(&term, "================").unwrap();
    writeln!(&term, "Starting Download...").unwrap();
    if let Some(full_length) = full_length {
        writeln!(&term, "Downloading: {}", HumanBytes(*full_length)).unwrap();
    } else {
        writeln!(&term, "Content-Length not specified!").unwrap();
    }
    if let Some(length) = length {
        writeln!(&term, "Remaining  : {}", HumanBytes(length)).unwrap();
    }
    writeln!(&term, "Offset     : {}", HumanBytes(*offset)).unwrap();

    bar.set_message("Downloading...");

    if let Some(content_length) = full_length {
        bar.set_style(PROGRESS_STYLE.clone());
        bar.set_length(*content_length);
        bar.set_position(*offset);
    }

    let mut stream = res.bytes_stream();

    while let Some(item) = timeout(connection_timeout, stream.next())
        .await
        .context("Chunk download timeout")?
    {
        let chunk: Bytes = item.context("Error downloading byte chunk")?;

        output
            .write_all(chunk.chunk())
            .await
            .context("Error writing to file")?;

        let len = chunk.len() as u64;
        *offset += len;
        downloaded += len;
        bar.set_position(*offset);
    }

    if length.is_some_and(|&length| downloaded < length) {
        bail!("Incomplete download");
    } else {
        writeln!(&term, "Download complete!").unwrap();
        writeln!(&term, "Downloaded: {}", HumanBytes(*offset)).unwrap();
        *incomplete = false;
    }

    Ok(())
}
