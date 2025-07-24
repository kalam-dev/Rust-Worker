use worker::*;
use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::time::Duration;

// Request payload for the API
#[derive(Deserialize)]
struct ScrapeRequest {
    url: String,
}

// Response structure for the API
#[derive(Serialize)]
struct ScrapeResponse {
    success: bool,
    files: Vec<String>,
    error: Option<String>,
}

// Structure for Cloudflare Browser Rendering API responses
#[derive(Deserialize)]
struct BrowserRenderingResponse<T> {
    success: bool,
    result: T,
}

// Main event handler for Cloudflare Workers
#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    // Only accept POST requests
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }

    // Parse the request body
    let scrape_request: ScrapeRequest = match req.json().await {
        Ok(data) => data,
        Err(e) => return Response::from_json(&ScrapeResponse {
            success: false,
            files: vec![],
            error: Some(format!("Invalid request body: {}", e)),
        }).map(|r| r.with_status(400)),
    };

    // Get API token and account ID from environment
    let api_token = match env.secret("CLOUDFLARE_API_TOKEN") {
        Ok(token) => token.to_string(),
        Err(e) => return Response::from_json(&ScrapeResponse {
            success: false,
            files: vec![],
            error: Some(format!("Missing API token: {}", e)),
        }).map(|r| r.with_status(500)),
    };
    let account_id = match env.secret("CLOUDFLARE_ACCOUNT_ID") {
        Ok(id) => id.to_string(),
        Err(e) => return Response::from_json(&ScrapeResponse {
            success: false,
            files: vec![],
            error: Some(format!("Missing account ID: {}", e)),
        }).map(|r| r.with_status(500)),
    };

    // Initialize R2 bucket
    let bucket = match env.bucket("SCRAPER_BUCKET") {
        Ok(bucket) => bucket,
        Err(e) => return Response::from_json(&ScrapeResponse {
            success: false,
            files: vec![],
            error: Some(format!("Failed to access R2 bucket: {}", e)),
        }).map(|r| r.with_status(500)),
    };

    // Perform the scrape
    match scrape_and_store(&scrape_request.url, &api_token, &account_id, bucket).await {
        Ok(files) => Response::from_json(&ScrapeResponse {
            success: true,
            files,
            error: None,
        }),
        Err(e) => Response::from_json(&ScrapeResponse {
            success: false,
            files: vec![],
            error: Some(format!("Scraping failed: {}", e)),
        }).map(|r| r.with_status(500)),
    }
}

// Main scraping and storage logic
async fn scrape_and_store(url: &str, api_token: &str, account_id: &str, bucket: Bucket) -> Result<Vec<String>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Cloudflare-Worker-Scraper/1.0")
        .build()
        .map_err(|e| worker::Error::RustError(format!("Failed to create client: {}", e)))?;

    // Fetch links
    let links = fetch_links(&client, url, api_token, account_id)
        .await
        .map_err(|e| worker::Error::RustError(format!("Failed to fetch links: {}", e)))?;

    let mut file_urls = vec![];

    // Fetch Markdown and store for each link
    for link in links {
        match fetch_markdown(&client, &link, api_token, account_id).await {
            Ok(markdown) => {
                // Generate a unique file name (e.g., based on URL and timestamp)
                let file_name = format!(
                    "markdown/{}.md",
                    url_to_filename(&link)
                );

                // Store in R2
                let file_content = markdown.as_bytes();
                bucket
                    .put(&file_name, file_content)
                    .execute()
                    .await
                    .map_err(|e| worker::Error::RustError(format!("Failed to store {}: {}", file_name, e)))?;

                // Construct public URL (assuming R2 bucket is publicly accessible)
                let public_url = format!("https://<your-r2-bucket-public-domain>/{file_name}");
                file_urls.push(public_url);
            }
            Err(e) => {
                console_log!("Failed to fetch Markdown for {}: {}", link, e);
                continue; // Continue with other links
            }
        }
    }

    Ok(file_urls)
}

// Fetch links using Browser Rendering API
async fn fetch_links(client: &Client, url: &str, api_token: &str, account_id: &str) -> Result<Vec<String>> {
    let api_url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/browser-rendering/links",
        account_id
    );

    let response = retry_request(|| {
        client
            .post(&api_url)
            .header("Authorization", format!("Bearer {}", api_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "url": url }))
            .send()
    })
    .await
    .map_err(|e| worker::Error::RustError(format!("Links request failed: {}", e)))?;

    let json: BrowserRenderingResponse<Vec<String>> = response
        .json()
        .await
        .map_err(|e| worker::Error::RustError(format!("Failed to parse links response: {}", e)))?;

    if !json.success {
        return Err(worker::Error::RustError("Links API returned success: false".to_string()));
    }

    Ok(json.result)
}

// Fetch Markdown using Browser Rendering API
async fn fetch_markdown(client: &Client, url: &str, api_token: &str, account_id: &str) -> Result<String> {
    let api_url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/browser-rendering/markdown",
        account_id
    );

    let response = retry_request(|| {
        client
            .post(&api_url)
            .header("Authorization", format!("Bearer {}", api_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "url": url }))
            .send()
    })
    .await
    .map_err(|e| worker::Error::RustError(format!("Markdown request failed: {}", e)))?;

    let json: BrowserRenderingResponse<String> = response
        .json()
        .await
        .map_err(|e| worker::Error::RustError(format!("Failed to parse markdown response: {}", e)))?;

    if !json.success {
        return Err(worker::Error::RustError("Markdown API returned success: false".to_string()));
    }

    Ok(json.result)
}

// Retry logic for HTTP requests
async fn retry_request<F, Fut>(mut request: F) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let max_retries = 3;
    let retry_delay = Duration::from_secs(2);
    let mut attempt = 0;

    loop {
        match request().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                if attempt >= max_retries {
                    return Err(worker::Error::RustError(format!(
                        "HTTP error after {} attempts: {}",
                        max_retries,
                        response.status()
                    )));
                }
            }
            Err(e) => {
                if attempt >= max_retries {
                    return Err(worker::Error::RustError(format!(
                        "Request failed after {} attempts: {}",
                        max_retries, e
                    )));
                }
            }
        }
        attempt += 1;
        console_log!("Retry attempt {} for request", attempt);
        worker::Delay::from(retry_delay).await;
    }
}

// Helper to convert URL to a safe filename
fn url_to_filename(url: &str) -> String {
    let safe_url = url.replace("://", "_").replace("/", "_").replace(".", "_");
    format!("{}_{}", safe_url, chrono::Utc::now().timestamp_millis())
}
