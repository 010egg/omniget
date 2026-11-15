//! Hidden-webview runner for the Douyin web fallback.
//!
//! When yt-dlp cannot read a Douyin video, the downloader in omniget-core
//! asks this provider to load the video page in a hidden window and run a
//! small script there. The script hands its result back by navigating to
//! `WEB_RESULT_HOST`; that navigation is cancelled and its query string read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use omniget_core::platforms::douyin::{DouyinWebProvider, WEB_RESULT_HOST};

/// Stops the page from playing video or sound while hidden.
const MUTE_SCRIPT: &str = r#"(function(){
  try {
    HTMLMediaElement.prototype.play = function() { this.muted = true; return Promise.resolve(); };
  } catch (e) {}
})();"#;

const PAGE_SETTLE: Duration = Duration::from_secs(3);
const RETRY_DELAY: Duration = Duration::from_millis(1500);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(45);
/// The page script retries its fetch for up to ~6s; past that, assume the
/// script was lost to a page navigation.
const SCRIPT_RERUN_AFTER: Duration = Duration::from_secs(8);

enum Outcome {
    Data(String),
    Retry(String),
    Error(String),
}

pub struct DesktopDouyinWebProvider {
    app: tauri::AppHandle,
    /// One page at a time: each run opens and closes its own window.
    lock: tokio::sync::Mutex<()>,
    counter: AtomicU64,
}

impl DesktopDouyinWebProvider {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            lock: tokio::sync::Mutex::new(()),
            counter: AtomicU64::new(0),
        }
    }

    async fn run(
        &self,
        window: &tauri::WebviewWindow,
        script: &str,
        slot: &Arc<Mutex<Option<Outcome>>>,
    ) -> anyhow::Result<String> {
        let start = Instant::now();
        tokio::time::sleep(PAGE_SETTLE).await;
        let mut last_retry = String::new();
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            *slot.lock().unwrap() = None;
            tracing::info!(
                "[douyin-web] running page script (attempt {}, {:?} elapsed)",
                attempt,
                start.elapsed()
            );
            window
                .eval(script)
                .map_err(|e| anyhow!("Douyin page script failed to start: {}", e))?;
            let attempt_start = Instant::now();

            loop {
                if let Some(outcome) = slot.lock().unwrap().take() {
                    match outcome {
                        Outcome::Data(data) => return Ok(data),
                        Outcome::Error(msg) => return Err(anyhow!("Douyin page: {}", msg)),
                        Outcome::Retry(msg) => {
                            tracing::info!("[douyin-web] page not ready: {}", msg);
                            last_retry = msg;
                            break;
                        }
                    }
                }
                // A script injected while the page is still navigating is
                // dropped with the old document, so inject it again.
                if attempt_start.elapsed() > SCRIPT_RERUN_AFTER {
                    last_retry = "no answer from page script".to_string();
                    break;
                }
                if start.elapsed() > TOTAL_TIMEOUT {
                    return Err(anyhow!(
                        "Timed out waiting for the Douyin page{}",
                        if last_retry.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", last_retry)
                        }
                    ));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }

            tokio::time::sleep(RETRY_DELAY).await;
        }
    }
}

#[async_trait]
impl DouyinWebProvider for DesktopDouyinWebProvider {
    async fn run_page_script(&self, page_url: &str, script: &str) -> anyhow::Result<String> {
        let parsed: url::Url = page_url
            .parse()
            .map_err(|e| anyhow!("Bad Douyin page URL: {}", e))?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        if host != "douyin.com" && !host.ends_with(".douyin.com") {
            return Err(anyhow!("Refusing to open a non-Douyin page"));
        }

        let _guard = self.lock.lock().await;
        let label = format!(
            "douyin-web-{}",
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
        let store = slot.clone();

        let window =
            tauri::WebviewWindowBuilder::new(&self.app, &label, tauri::WebviewUrl::External(parsed))
                .visible(false)
                .focused(false)
                .skip_taskbar(true)
                .inner_size(400.0, 300.0)
                .user_agent(
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36",
                )
                .initialization_script(MUTE_SCRIPT)
                .on_navigation(move |url| {
                    if url.host_str() != Some(WEB_RESULT_HOST) {
                        tracing::info!(
                            "[douyin-web] page navigating to {}{}",
                            url.host_str().unwrap_or(url.scheme()),
                            url.path()
                        );
                        return true;
                    }
                    for (key, value) in url.query_pairs() {
                        let outcome = match key.as_ref() {
                            "data" => Outcome::Data(value.to_string()),
                            "retry" => Outcome::Retry(value.to_string()),
                            "error" => Outcome::Error(value.to_string()),
                            _ => continue,
                        };
                        *store.lock().unwrap() = Some(outcome);
                        break;
                    }
                    false
                })
                .build()
                .map_err(|e| anyhow!("Failed to open Douyin page: {}", e))?;

        let result = self.run(&window, script, &slot).await;
        let _ = window.destroy();
        result
    }
}
