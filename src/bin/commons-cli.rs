use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use telegram_wikimedia_commons_bot::commons::CommonsClient;
use telegram_wikimedia_commons_bot::config::Config;
use telegram_wikimedia_commons_bot::models::{FileHit, Intent, Preferences, SearchQuery};
use telegram_wikimedia_commons_bot::parser::parse_intent;
use telegram_wikimedia_commons_bot::telegram::human_bytes;

/// CLI for searching and downloading Wikimedia Commons media.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Search query using the same syntax as the Telegram bot.
    #[arg(allow_hyphen_values = true)]
    query: Vec<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    format: OutputFormat,

    /// Disable spinner/animation for scripts.
    #[arg(long)]
    no_animation: bool,

    /// Bypass Telegram's 50 MB filter in CLI mode.
    #[arg(long)]
    bypass_50mb_limit: bool,

    /// Sort final results by file size.
    #[arg(long)]
    sort_size: bool,

    /// Download direct files in a category.
    #[arg(long)]
    download_category: Option<String>,

    /// Include subcategories when downloading a category.
    #[arg(long)]
    recursive: bool,

    /// Download destination directory.
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,

    /// Try to show image previews in Kitty-compatible terminals.
    #[arg(long)]
    preview: bool,

    /// Play audio results with mpv.
    #[arg(long)]
    play_audio: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Pretty,
    Json,
    Jsonl,
    Tsv,
}

/// Runs the Commons CLI.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::from_env();
    let commons = CommonsClient::new(&config)?;
    let query_text = cli.query.join(" ");

    if let Some(category) = &cli.download_category {
        show_spinner(&cli, "Loading category").await;
        let category_info = commons
            .category_info(category, 20, if cli.recursive { 20 } else { 0 }, u64::MAX)
            .await?;
        download_files(&commons, &category_info.files, &cli.output_dir).await?;
        if cli.recursive {
            for subcategory in category_info.subcategories {
                let info = commons
                    .category_info(&subcategory.display_title, 20, 0, u64::MAX)
                    .await?;
                download_files(&commons, &info.files, &cli.output_dir).await?;
            }
        }
        return Ok(());
    }

    let mut search_query = match parse_intent(&query_text) {
        Intent::FileSearch(query) => query,
        Intent::CategorySearch(query) => {
            let categories = commons.search_categories(&query, 20).await?;
            for category in categories {
                println!("{}\t{}", category.page_id, category.title);
            }
            return Ok(());
        }
        Intent::Help | Intent::Preferences | Intent::Stats | Intent::Empty => {
            SearchQuery::default()
        }
    };
    if cli.bypass_50mb_limit {
        search_query.bypass_telegram_limit = true;
    }
    if cli.sort_size {
        search_query.sort_by_size = true;
    }

    show_spinner(&cli, "Searching Commons").await;
    let max_file_bytes = if cli.bypass_50mb_limit {
        u64::MAX
    } else {
        config.max_file_bytes
    };
    let files = commons
        .search_files(&search_query, &Preferences::default(), 20, max_file_bytes)
        .await?;

    render_files(&files, cli.format)?;

    if cli.preview {
        preview_images(&files).await.ok();
    }
    if cli.play_audio {
        play_audio(&files).await.ok();
    }

    Ok(())
}

/// Renders files in the selected CLI output format.
fn render_files(files: &[FileHit], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Pretty => {
            for file in files {
                println!(
                    "{}  {}  {}",
                    file.page_id,
                    human_bytes(file.size_bytes),
                    file.description_url.as_deref().unwrap_or(&file.title)
                );
            }
        }
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(files)?),
        OutputFormat::Jsonl => {
            for file in files {
                println!("{}", serde_json::to_string(file)?);
            }
        }
        OutputFormat::Tsv => {
            for file in files {
                println!(
                    "{}\t{}\t{}\t{}",
                    file.page_id,
                    file.size_bytes,
                    file.file_name,
                    file.url.as_deref().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

/// Downloads a list of Commons files to a directory.
async fn download_files(
    commons: &CommonsClient,
    files: &[FileHit],
    output_dir: &Path,
) -> Result<()> {
    tokio::fs::create_dir_all(output_dir).await?;
    for file in files {
        let path = output_dir.join(sanitize_filename(&file.file_name));
        let bytes = commons.download_file(file).await?;
        tokio::fs::write(&path, bytes).await?;
        println!("Downloaded {}", path.display());
    }
    Ok(())
}

/// Shows a tiny spinner marker unless script mode is requested.
async fn show_spinner(cli: &Cli, label: &str) {
    if cli.no_animation || cli.format != OutputFormat::Pretty {
        return;
    }
    eprintln!("{label}...");
}

/// Tries to preview image URLs in Kitty terminals.
async fn preview_images(files: &[FileHit]) -> Result<()> {
    for file in files.iter().take(5) {
        if !file
            .mime
            .as_deref()
            .is_some_and(|mime| mime.starts_with("image/"))
        {
            continue;
        }
        let url = file
            .thumb_url
            .as_ref()
            .or(file.url.as_ref())
            .context("no image URL")?;
        let mut child = tokio::process::Command::new("kitty")
            .arg("+kitten")
            .arg("icat")
            .arg(url)
            .stdout(Stdio::inherit())
            .stderr(Stdio::null())
            .spawn()?;
        child.wait().await?;
    }
    Ok(())
}

/// Plays the first audio result through mpv.
async fn play_audio(files: &[FileHit]) -> Result<()> {
    let file = files
        .iter()
        .find(|file| file.is_audio())
        .context("no audio file in results")?;
    let url = file.url.as_ref().context("audio file has no URL")?;
    let mut child = tokio::process::Command::new("mpv")
        .arg(url)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    child.wait().await?;
    Ok(())
}

/// Sanitizes a Commons file name for local filesystem use.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|ch| if matches!(ch, '/' | '\0') { '_' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Cli, OutputFormat, play_audio, preview_images, sanitize_filename};
    use clap::Parser;
    use telegram_wikimedia_commons_bot::models::FileHit;

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(sanitize_filename("a/b.jpg"), "a_b.jpg");
        assert_eq!(sanitize_filename("nul\0name.jpg"), "nul_name.jpg");
    }

    #[test]
    fn parses_hyphen_prefixed_query_terms() {
        let cli = Cli::parse_from(["commons-cli", "minsk", "-img"]);
        assert_eq!(cli.query, vec!["minsk", "-img"]);
    }

    #[test]
    fn parses_script_download_and_media_flags() {
        let cli = Cli::parse_from([
            "commons-cli",
            "--format",
            "jsonl",
            "--no-animation",
            "--bypass-50mb-limit",
            "--sort-size",
            "--download-category",
            "Minsk",
            "--recursive",
            "--output-dir",
            "out",
            "--preview",
            "--play-audio",
            "bird",
        ]);

        assert_eq!(cli.query, vec!["bird"]);
        assert_eq!(cli.format, OutputFormat::Jsonl);
        assert!(cli.no_animation);
        assert!(cli.bypass_50mb_limit);
        assert!(cli.sort_size);
        assert_eq!(cli.download_category.as_deref(), Some("Minsk"));
        assert!(cli.recursive);
        assert_eq!(cli.output_dir, std::path::PathBuf::from("out"));
        assert!(cli.preview);
        assert!(cli.play_audio);
    }

    #[tokio::test]
    async fn preview_images_ignores_empty_and_non_image_results() {
        preview_images(&[]).await.unwrap();
        preview_images(&[FileHit {
            mime: Some("audio/ogg".into()),
            ..FileHit::default()
        }])
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn play_audio_reports_missing_audio_and_missing_url() {
        let error = play_audio(&[]).await.unwrap_err();
        assert!(error.to_string().contains("no audio file"));

        let error = play_audio(&[FileHit {
            mime: Some("audio/ogg".into()),
            ..FileHit::default()
        }])
        .await
        .unwrap_err();
        assert!(error.to_string().contains("audio file has no URL"));
    }
}
