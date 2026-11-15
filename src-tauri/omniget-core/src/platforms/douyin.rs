use crate::models::progress::ProgressUpdate;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::core::direct_downloader;
use crate::core::http_client;
use crate::core::ytdlp;
use crate::models::media::{
    DownloadOptions, DownloadResult, MediaInfo, MediaType, VideoQuality as MediaVideoQuality,
};
use crate::platforms::traits::PlatformDownloader;

const DOUYIN_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36";
const DOUYIN_REFERER: &str = "https://www.douyin.com/?recommend=1";

/// Quality format marker for videos resolved through the web fallback.
/// `download` sees it and skips yt-dlp.
const WEB_FORMAT: &str = "douyin-web";

/// Host the page script navigates to in order to hand its result back.
/// The webview cancels that navigation and reads the query string.
pub const WEB_RESULT_HOST: &str = "omniget-douyin-result.local";

/// Runs Douyin's own web API inside a real Douyin page.
///
/// yt-dlp's Douyin extractor breaks whenever Douyin changes its anti-bot
/// checks. A page loaded in a webview passes those checks the same way a
/// normal browser does, so the desktop app provides one as a fallback.
#[async_trait]
pub trait DouyinWebProvider: Send + Sync {
    /// Load `page_url`, run `script` in it and return the `data` value the
    /// script sends to [`WEB_RESULT_HOST`]. A `retry` value means the page
    /// was not ready yet and the script should be run again; an `error`
    /// value is a failure.
    async fn run_page_script(&self, page_url: &str, script: &str) -> anyhow::Result<String>;
}

static WEB_PROVIDER: OnceLock<Arc<dyn DouyinWebProvider>> = OnceLock::new();

pub fn set_web_provider(provider: Arc<dyn DouyinWebProvider>) {
    let _ = WEB_PROVIDER.set(provider);
}

fn web_provider() -> Option<&'static Arc<dyn DouyinWebProvider>> {
    WEB_PROVIDER.get()
}

/// Script run inside a Douyin page. It calls the same detail endpoint the
/// page itself uses and sends back only the fields we need, so the result
/// stays small enough to travel in a URL.
pub fn detail_script(aweme_id: &str) -> String {
    const TEMPLATE: &str = r#"(function(){
  var RESULT = 'https://__HOST__/';
  function send(k, v) { window.location.href = RESULT + '?' + k + '=' + encodeURIComponent(v); }
  if (!/(^|\.)douyin\.com$/.test(location.hostname)) { send('retry', 'page not ready: ' + location.hostname); return; }
  var url = '/aweme/v1/web/aweme/detail/?device_platform=webapp&aid=6383&aweme_id=__ID__';
  var attempt = 0;
  function again(msg) { if (attempt < 4) { setTimeout(go, 1500); } else { send('error', msg); } }
  function go() {
    attempt++;
    fetch(url, { credentials: 'include', headers: { 'Accept': 'application/json, text/plain, */*' } })
      .then(function(r) { return r.text(); })
      .then(function(t) {
        var j = null; try { j = JSON.parse(t); } catch (e) {}
        var d = j && j.aweme_detail;
        if (!d) {
          var reason = j && j.filter_detail && j.filter_detail.filter_reason;
          again(reason || ('empty response (' + t.length + ' bytes)'));
          return;
        }
        var v = d.video || {};
        var variants = (v.bit_rate || []).map(function(b) {
          var p = b.play_addr || {};
          return { gear: b.gear_name || '', bitrate: b.bit_rate || 0, width: p.width || 0,
                   height: p.height || 0, h265: !!b.is_h265, urls: (p.url_list || []).slice(0, 3) };
        });
        if (!variants.length && v.play_addr) {
          variants.push({ gear: '', bitrate: 0, width: v.width || 0, height: v.height || 0,
                          h265: false, urls: (v.play_addr.url_list || []).slice(0, 3) });
        }
        send('data', JSON.stringify({
          id: d.aweme_id || '', desc: d.desc || '',
          author: (d.author && d.author.nickname) || '',
          duration_ms: v.duration || d.duration || 0,
          cover: (v.cover && v.cover.url_list && v.cover.url_list[0]) || '',
          images: (d.images || []).length,
          variants: variants
        }));
      })
      .catch(function(e) { again(String((e && e.message) || e)); });
  }
  go();
})();"#;
    TEMPLATE
        .replace("__HOST__", WEB_RESULT_HOST)
        .replace("__ID__", aweme_id)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebDetail {
    pub id: String,
    pub desc: String,
    pub author: String,
    pub duration_ms: u64,
    pub cover: String,
    pub images: u32,
    pub variants: Vec<WebVariant>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebVariant {
    pub gear: String,
    pub bitrate: u64,
    pub width: u32,
    pub height: u32,
    pub h265: bool,
    pub urls: Vec<String>,
}

impl WebVariant {
    /// The "1080p"-style number: the short side, so portrait videos are
    /// labelled the way Douyin labels them. Falls back to the gear name
    /// (e.g. `normal_1080_0`) when the size is missing.
    pub fn resolution(&self) -> u32 {
        if self.width > 0 && self.height > 0 {
            return self.width.min(self.height);
        }
        self.gear
            .split('_')
            .find_map(|part| part.parse::<u32>().ok().filter(|n| *n >= 144))
            .unwrap_or(0)
    }
}

/// Pick the best playable variant at or below `target` (a short-side
/// resolution). H.264 is preferred over H.265 at the same resolution because
/// more players handle it. With no variant at or below the target, the
/// smallest one is used.
pub fn pick_variant(variants: &[WebVariant], target: Option<u32>) -> Option<&WebVariant> {
    let mut playable: Vec<&WebVariant> = variants.iter().filter(|v| !v.urls.is_empty()).collect();
    playable.sort_by(|a, b| {
        b.resolution()
            .cmp(&a.resolution())
            .then(a.h265.cmp(&b.h265))
            .then(b.bitrate.cmp(&a.bitrate))
    });
    match target {
        Some(t) if t > 0 => playable
            .iter()
            .find(|v| v.resolution() <= t)
            .or_else(|| playable.last())
            .copied(),
        _ => playable.first().copied(),
    }
}

fn quality_label(height: u32) -> String {
    match height {
        2160 => "4K".to_string(),
        1440 => "2K".to_string(),
        _ => format!("{}p", height),
    }
}

fn aweme_id_from_canonical(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["video", id] if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => {
            Some(id.to_string())
        }
        _ => None,
    }
}

/// Short, file-name friendly title: the caption without hashtags.
fn clean_title(desc: &str) -> String {
    let without_tags: Vec<&str> = desc
        .split_whitespace()
        .filter(|w| !w.starts_with('#') && !w.starts_with('@'))
        .collect();
    let joined = without_tags.join(" ");
    let trimmed: String = joined.chars().take(80).collect();
    if trimmed.trim().is_empty() {
        "Douyin Video".to_string()
    } else {
        trimmed.trim().to_string()
    }
}

fn parse_web_detail(raw: &str) -> anyhow::Result<WebDetail> {
    serde_json::from_str(raw).map_err(|e| anyhow!("Douyin web fallback returned bad data: {}", e))
}

fn media_info_from_web(detail: &WebDetail, canonical: &str) -> anyhow::Result<MediaInfo> {
    if detail.variants.iter().all(|v| v.urls.is_empty()) {
        if detail.images > 0 {
            return Err(anyhow!(
                "Douyin web fallback does not support image posts yet"
            ));
        }
        return Err(anyhow!("Douyin web fallback found no playable video"));
    }

    let mut seen = HashSet::new();
    let mut variants: Vec<&WebVariant> = detail
        .variants
        .iter()
        .filter(|v| !v.urls.is_empty())
        .collect();
    variants.sort_by_key(|v| std::cmp::Reverse(v.resolution()));
    let mut qualities = Vec::new();
    for v in variants {
        let res = v.resolution();
        if !seen.insert(res) {
            continue;
        }
        qualities.push(MediaVideoQuality {
            label: if res > 0 {
                quality_label(res)
            } else {
                "best".to_string()
            },
            width: v.width,
            height: res,
            url: canonical.to_string(),
            format: WEB_FORMAT.to_string(),
        });
    }

    Ok(MediaInfo {
        title: clean_title(&detail.desc),
        author: if detail.author.is_empty() {
            "Unknown".to_string()
        } else {
            detail.author.clone()
        },
        platform: "douyin".to_string(),
        duration_seconds: (detail.duration_ms > 0).then(|| detail.duration_ms as f64 / 1000.0),
        thumbnail_url: (!detail.cover.is_empty()).then(|| detail.cover.clone()),
        available_qualities: qualities,
        media_type: MediaType::Video,
        file_size_bytes: None,
    })
}

pub struct DouyinDownloader;

impl Default for DouyinDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl DouyinDownloader {
    pub fn new() -> Self {
        Self
    }

    fn extra_flags() -> Vec<String> {
        vec![
            "--referer".to_string(),
            DOUYIN_REFERER.to_string(),
            "--user-agent".to_string(),
            DOUYIN_UA.to_string(),
        ]
    }

    fn host_matches(host: &str) -> bool {
        let host = host.to_lowercase();
        host == "douyin.com"
            || host.ends_with(".douyin.com")
            || host == "iesdouyin.com"
            || host.ends_with(".iesdouyin.com")
            || host == "amemv.com"
            || host.ends_with(".amemv.com")
    }

    fn is_short_link(url: &str) -> bool {
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                let host = host.to_lowercase();
                return host == "v.douyin.com"
                    || host == "amemv.com"
                    || host.ends_with(".amemv.com")
                    || host == "iesdouyin.com"
                    || host.ends_with(".iesdouyin.com");
            }
        }
        false
    }

    fn canonicalize(url: &str) -> String {
        let Ok(parsed) = url::Url::parse(url) else {
            return url.to_string();
        };
        let host = parsed.host_str().unwrap_or("").to_lowercase();
        if !Self::host_matches(&host) {
            return url.to_string();
        }

        let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
        let target = match segments.as_slice() {
            ["share", kind @ ("video" | "note" | "slides"), id, ..] => Some((*kind, *id)),
            [kind @ ("video" | "note"), id, ..] => Some((*kind, *id)),
            _ => None,
        };
        if let Some((kind, id)) = target {
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                let kind = if kind == "slides" { "note" } else { kind };
                return format!("https://www.douyin.com/{}/{}", kind, id);
            }
        }

        if let Some(id) = parsed
            .query_pairs()
            .find(|(k, _)| k == "modal_id")
            .map(|(_, v)| v.to_string())
        {
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                return format!("https://www.douyin.com/video/{}", id);
            }
        }

        url.to_string()
    }

    async fn resolve_url(url: &str) -> String {
        if !Self::is_short_link(url) {
            return Self::canonicalize(url);
        }
        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent(DOUYIN_UA);
        let client = match http_client::apply_global_proxy(builder).build() {
            Ok(c) => c,
            Err(_) => return Self::canonicalize(url),
        };
        match client.get(url).send().await {
            Ok(resp) => Self::canonicalize(resp.url().as_str()),
            Err(e) => {
                tracing::warn!("[douyin] short-link resolve failed: {}", e);
                Self::canonicalize(url)
            }
        }
    }

    fn build_qualities(formats: &[serde_json::Value], url: &str) -> Vec<MediaVideoQuality> {
        let mut seen_heights = HashSet::new();
        let mut qualities = Vec::new();

        for fmt in formats {
            let height = fmt.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let vcodec = fmt.get("vcodec").and_then(|v| v.as_str()).unwrap_or("none");

            if height == 0 || vcodec == "none" {
                continue;
            }
            if !seen_heights.insert(height) {
                continue;
            }

            let label = match height {
                2160 => "4K".to_string(),
                1440 => "2K".to_string(),
                1080 => "1080p".to_string(),
                720 => "720p".to_string(),
                480 => "480p".to_string(),
                360 => "360p".to_string(),
                _ => format!("{}p", height),
            };

            qualities.push(MediaVideoQuality {
                label,
                width: fmt.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                height,
                url: url.to_string(),
                format: "mp4".to_string(),
            });
        }

        qualities.sort_by(|a, b| b.height.cmp(&a.height));

        if qualities.is_empty() {
            qualities.push(MediaVideoQuality {
                label: "best".to_string(),
                width: 0,
                height: 0,
                url: url.to_string(),
                format: "mp4".to_string(),
            });
        }

        qualities
    }

    async fn fetch_web_detail(canonical: &str) -> anyhow::Result<WebDetail> {
        let provider =
            web_provider().ok_or_else(|| anyhow!("Douyin web fallback is not available"))?;
        let id = aweme_id_from_canonical(canonical)
            .ok_or_else(|| anyhow!("Douyin web fallback only handles video links"))?;
        let raw = provider
            .run_page_script(canonical, &detail_script(&id))
            .await?;
        parse_web_detail(&raw)
    }

    async fn ytdlp_media_info(resolved: &str) -> anyhow::Result<MediaInfo> {
        let ytdlp_path = ytdlp::find_ytdlp_cached()
            .await
            .ok_or_else(|| anyhow!("yt-dlp not found"))?;

        let extra = Self::extra_flags();
        let json = ytdlp::get_video_info(&ytdlp_path, resolved, &extra).await?;

        let title = json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Douyin Video")
            .to_string();

        let author = json
            .get("uploader")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("channel").and_then(|v| v.as_str()))
            .unwrap_or("Unknown")
            .to_string();

        let duration = json.get("duration").and_then(|v| v.as_f64());

        let thumbnail = json
            .get("thumbnail")
            .and_then(|v| v.as_str())
            .map(String::from);

        let formats = json
            .get("formats")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let qualities = Self::build_qualities(&formats, resolved);

        let has_video = formats
            .iter()
            .any(|f| f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("none") != "none");

        Ok(MediaInfo {
            title,
            author,
            platform: "douyin".to_string(),
            duration_seconds: duration,
            thumbnail_url: thumbnail,
            available_qualities: qualities,
            media_type: if has_video {
                MediaType::Video
            } else {
                MediaType::Audio
            },
            file_size_bytes: None,
        })
    }

    async fn download_web(
        info: &MediaInfo,
        canonical: &str,
        opts: &DownloadOptions,
        progress: mpsc::Sender<ProgressUpdate>,
    ) -> anyhow::Result<DownloadResult> {
        // Play URLs expire, so fetch fresh ones instead of reusing the ones
        // seen when the media info was built.
        let detail = Self::fetch_web_detail(canonical).await?;
        let target = opts
            .quality
            .as_ref()
            .and_then(|q| q.trim_end_matches('p').parse::<u32>().ok());
        let variant = pick_variant(&detail.variants, target)
            .ok_or_else(|| anyhow!("Douyin web fallback found no playable video"))?;

        let id = aweme_id_from_canonical(canonical).unwrap_or_else(|| detail.id.clone());
        let filename = format!("{} - {} [{}].mp4", info.author, info.title, id);
        let output_path = opts.output_dir.join(sanitize_filename::sanitize(&filename));

        let builder = reqwest::Client::builder().user_agent(DOUYIN_UA);
        let client = http_client::apply_global_proxy(builder).build()?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::REFERER,
            reqwest::header::HeaderValue::from_static("https://www.douyin.com/"),
        );

        let mut last_err = None;
        for url in &variant.urls {
            match direct_downloader::download_direct_with_headers(
                &client,
                url,
                &output_path,
                progress.clone(),
                Some(headers.clone()),
                Some(&opts.cancel_token),
            )
            .await
            {
                Ok(bytes) => {
                    return Ok(DownloadResult {
                        file_path: output_path,
                        file_size_bytes: bytes,
                        duration_seconds: info.duration_seconds.unwrap_or(0.0),
                        torrent_id: None,
                    });
                }
                Err(e) => {
                    if opts.cancel_token.is_cancelled() {
                        return Err(e);
                    }
                    tracing::warn!("[douyin] web fallback mirror failed: {}", e);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("Douyin web fallback found no playable video")))
    }
}

#[async_trait]
impl PlatformDownloader for DouyinDownloader {
    fn name(&self) -> &str {
        "douyin"
    }

    fn can_handle(&self, url: &str) -> bool {
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                return Self::host_matches(host);
            }
        }
        false
    }

    async fn get_media_info(&self, url: &str) -> anyhow::Result<MediaInfo> {
        let resolved = Self::resolve_url(url).await;
        let ytdlp_err = match Self::ytdlp_media_info(&resolved).await {
            Ok(info) => return Ok(info),
            Err(e) => e,
        };

        if web_provider().is_none() || aweme_id_from_canonical(&resolved).is_none() {
            return Err(ytdlp_err);
        }
        tracing::warn!("[douyin] yt-dlp failed, trying web fallback: {}", ytdlp_err);
        match Self::fetch_web_detail(&resolved)
            .await
            .and_then(|detail| media_info_from_web(&detail, &resolved))
        {
            Ok(info) => Ok(info),
            Err(web_err) => Err(anyhow!(
                "{} (web fallback also failed: {})",
                ytdlp_err,
                web_err
            )),
        }
    }

    async fn download(
        &self,
        info: &MediaInfo,
        opts: &DownloadOptions,
        progress: mpsc::Sender<ProgressUpdate>,
    ) -> anyhow::Result<DownloadResult> {
        let _ = progress.send(ProgressUpdate::percent(0.0)).await;

        if let Some(first) = info.available_qualities.first() {
            if first.format == WEB_FORMAT {
                let canonical = first.url.clone();
                return Self::download_web(info, &canonical, opts, progress).await;
            }
        }

        let ytdlp_path = match &opts.ytdlp_path {
            Some(p) => p.clone(),
            None => ytdlp::find_ytdlp_cached()
                .await
                .ok_or_else(|| anyhow!("yt-dlp not found"))?,
        };

        let canonical = info
            .available_qualities
            .first()
            .map(|q| q.url.as_str())
            .unwrap_or("");
        if canonical.is_empty() {
            return Err(anyhow!("No URL available"));
        }
        let canonical = Self::resolve_url(canonical).await;

        let quality_height = opts
            .quality
            .as_ref()
            .and_then(|q| q.trim_end_matches('p').parse::<u32>().ok());

        let extra = Self::extra_flags();

        ytdlp::download_video(
            &ytdlp_path,
            &canonical,
            &opts.output_dir,
            quality_height,
            progress,
            opts.download_mode.as_deref(),
            opts.format_id.as_deref(),
            opts.filename_template.as_deref(),
            opts.referer.as_deref().or(Some(DOUYIN_REFERER)),
            opts.cancel_token.clone(),
            None,
            opts.concurrent_fragments,
            opts.download_subtitles,
            &extra,
            opts.audio_format.as_deref(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(gear: &str, w: u32, h: u32, bitrate: u64, h265: bool) -> WebVariant {
        WebVariant {
            gear: gear.to_string(),
            bitrate,
            width: w,
            height: h,
            h265,
            urls: vec![format!("https://cdn.example/{}", gear)],
        }
    }

    #[test]
    fn resolution_uses_short_side_then_gear_name() {
        assert_eq!(
            variant("normal_1080_0", 1080, 1920, 1, false).resolution(),
            1080
        );
        assert_eq!(variant("normal_720_0", 0, 0, 1, false).resolution(), 720);
        assert_eq!(variant("adapt_lowest", 0, 0, 1, false).resolution(), 0);
    }

    #[test]
    fn pick_variant_prefers_best_h264_under_target() {
        let vs = vec![
            variant("normal_540_0", 540, 960, 900, false),
            variant("normal_1080_0", 1080, 1920, 1300, true),
            variant("normal_1080_1", 1080, 1920, 1200, false),
            variant("normal_720_0", 720, 1280, 1000, false),
        ];
        assert_eq!(pick_variant(&vs, None).unwrap().gear, "normal_1080_1");
        assert_eq!(pick_variant(&vs, Some(720)).unwrap().gear, "normal_720_0");
        assert_eq!(pick_variant(&vs, Some(360)).unwrap().gear, "normal_540_0");
    }

    #[test]
    fn pick_variant_skips_variants_without_urls() {
        let mut empty = variant("normal_1080_0", 1080, 1920, 1, false);
        empty.urls.clear();
        let vs = vec![empty, variant("normal_720_0", 720, 1280, 1, false)];
        assert_eq!(pick_variant(&vs, None).unwrap().gear, "normal_720_0");
    }

    #[test]
    fn media_info_from_web_builds_marked_qualities() {
        let raw = r#"{"id":"7687559761327807673","desc":"我李逍遥可以对天发誓 #西施 #曜",
            "author":"裘球","duration_ms":19100,"cover":"https://p.example/c.jpg","images":0,
            "variants":[
              {"gear":"normal_1080_0","bitrate":1296626,"width":1080,"height":1920,"h265":false,"urls":["https://a"]},
              {"gear":"normal_720_0","bitrate":930113,"width":720,"height":1280,"h265":false,"urls":["https://b"]},
              {"gear":"low_720_0","bitrate":866162,"width":720,"height":1280,"h265":false,"urls":["https://c"]}
            ]}"#;
        let detail = parse_web_detail(raw).unwrap();
        let canonical = "https://www.douyin.com/video/7687559761327807673";
        let info = media_info_from_web(&detail, canonical).unwrap();
        assert_eq!(info.title, "我李逍遥可以对天发誓");
        assert_eq!(info.author, "裘球");
        assert_eq!(info.duration_seconds, Some(19.1));
        let labels: Vec<&str> = info
            .available_qualities
            .iter()
            .map(|q| q.label.as_str())
            .collect();
        assert_eq!(labels, vec!["1080p", "720p"]);
        assert!(info
            .available_qualities
            .iter()
            .all(|q| q.format == WEB_FORMAT && q.url == canonical));
    }

    #[test]
    fn media_info_from_web_rejects_image_posts() {
        let detail = parse_web_detail(r#"{"images":3,"variants":[]}"#).unwrap();
        let err = media_info_from_web(&detail, "https://www.douyin.com/video/1").unwrap_err();
        assert!(err.to_string().contains("image posts"));
    }

    #[test]
    fn aweme_id_only_from_video_links() {
        assert_eq!(
            aweme_id_from_canonical("https://www.douyin.com/video/123").as_deref(),
            Some("123")
        );
        assert_eq!(
            aweme_id_from_canonical("https://www.douyin.com/note/123"),
            None
        );
        assert_eq!(
            aweme_id_from_canonical("https://www.douyin.com/video/12a"),
            None
        );
    }

    #[test]
    fn detail_script_embeds_id_and_result_host() {
        let s = detail_script("7687559761327807673");
        assert!(s.contains("aweme_id=7687559761327807673"));
        assert!(s.contains(WEB_RESULT_HOST));
        assert!(!s.contains("__ID__") && !s.contains("__HOST__"));
    }

    #[test]
    fn clean_title_drops_tags_and_falls_back() {
        assert_eq!(clean_title("hello #tag @someone world"), "hello world");
        assert_eq!(clean_title("#only #tags"), "Douyin Video");
    }
}
