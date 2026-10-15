// メッセージ中の URL を取得して、AI に渡せる形(本文テキスト or 画像)にする
use base64::Engine;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

const MAX_URLS: usize = 3;
const MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 8000;
const MAX_REDIRECTS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(15);

pub enum WebContent {
    Text(String),
    Image(String),
}

// http(s):// で始まる部分を取り出す(`<URL>` の埋め込み抑止記法や末尾の句読点にも対応)
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    // 日本語の文章ではURLの直後にスペースがないことが多いので、全角の句読点や括弧でも区切る
    let is_separator = |c: char| {
        c.is_whitespace()
            || matches!(c, '<' | '>' | '「' | '」' | '『' | '』' | '（' | '）' | '、' | '。' | '！' | '？')
    };
    for word in text.split(is_separator) {
        let Some(start) = word.find("http://").or_else(|| word.find("https://")) else {
            continue;
        };
        let url = word[start..]
            .trim_end_matches(|c: char| matches!(c, '.' | ',' | ')' | ']' | '!' | '?' | '"' | '\''));
        if reqwest::Url::parse(url).is_ok() && !urls.iter().any(|u| u == url) {
            urls.push(url.to_string());
        }
        if urls.len() >= MAX_URLS {
            break;
        }
    }
    urls
}

pub async fn fetch(url: &str) -> Result<WebContent, String> {
    let mut url = reqwest::Url::parse(url).map_err(|_| "URLの形式が正しくありません".to_string())?;

    // リダイレクト先も毎回チェックするため、自前でたどる
    for _ in 0..=MAX_REDIRECTS {
        let res = request(&url).await?;
        if res.status().is_redirection() {
            let location = res
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("リダイレクト先がありません")?;
            url = url
                .join(location)
                .map_err(|_| "リダイレクト先のURLが不正です".to_string())?;
            continue;
        }
        if !res.status().is_success() {
            return Err(format!("ページを取得できませんでした ({})", res.status()));
        }
        return read_content(res).await;
    }
    Err("リダイレクトが多すぎます".to_string())
}

async fn request(url: &reqwest::Url) -> Result<reqwest::Response, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("http / https 以外のURLは開けません".to_string());
    }
    let host = url.host_str().ok_or("URLにホスト名がありません")?;
    let port = url.port_or_known_default().unwrap_or(443);

    // サーバー内部や LAN の機器にアクセスされないよう、公開IPだけを許可する
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|_| format!("{} が見つかりません", host))?
        .collect();
    let addr = *addrs.first().ok_or_else(|| format!("{} が見つかりません", host))?;
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err("このURLにはアクセスできません".to_string());
    }

    // チェックしたIPにそのまま接続する(DNSの再解決で別のIPに向けられるのを防ぐ)
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .resolve(host, addr)
        .user_agent("Mozilla/5.0 (compatible; ai-discord-bot)")
        .build()
        .map_err(|e| e.to_string())?;
    client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("ページを取得できませんでした: {}", e))
}

async fn read_content(mut res: reqwest::Response) -> Result<WebContent, String> {
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = content_type.split(';').next().unwrap_or("").trim().to_string();

    let is_html = mime == "text/html" || mime == "application/xhtml+xml";
    let is_text = mime.starts_with("text/") || mime == "application/json";
    let is_image = mime.starts_with("image/");
    if !is_html && !is_text && !is_image {
        return Err(format!("対応していない形式です ({})", mime));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| format!("ページの読み込みに失敗しました: {}", e))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_BYTES {
            return Err("ページが大きすぎます".to_string());
        }
    }

    if is_image {
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok(WebContent::Image(format!("data:{};base64,{}", mime, data)));
    }

    let text = decode(&bytes, &content_type);
    let text = if is_html { html_to_text(&text) } else { text };
    Ok(WebContent::Text(truncate(text.trim(), MAX_TEXT_CHARS)))
}

// 文字コードを判定してデコードする(Shift_JIS などの日本語サイト対策)
fn decode(bytes: &[u8], content_type: &str) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let label = charset_of(content_type).or_else(|| charset_of(&head));
    let encoding = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

fn charset_of(text: &str) -> Option<String> {
    let rest = &text[text.find("charset=")? + "charset=".len()..];
    let label: String = rest
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!label.is_empty()).then_some(label)
}

fn html_to_text(html: &str) -> String {
    html2text::config::plain()
        .link_footnotes(false)
        .string_from_read(html.as_bytes(), 200)
        .unwrap_or_default()
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars).collect();
    s.push_str("\n…(以下省略)");
    s
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)) // CGNAT
                || a >= 224) // マルチキャスト・予約済み
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // ユニークローカル
                || (first & 0xffc0) == 0xfe80) // リンクローカル
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_urls_from_text() {
        let urls = extract_urls("これ見て https://example.com/a?b=1。と <https://example.org/x> も");
        assert_eq!(urls, vec!["https://example.com/a?b=1", "https://example.org/x"]);
    }

    #[test]
    fn ignores_duplicates_and_limits_count() {
        let text = "https://a.com https://a.com https://b.com https://c.com https://d.com";
        assert_eq!(extract_urls(text), vec!["https://a.com", "https://b.com", "https://c.com"]);
    }

    #[test]
    fn no_urls() {
        assert!(extract_urls("こんにちは http なし").is_empty());
    }

    #[test]
    fn blocks_private_ips() {
        for ip in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "::ffff:127.0.0.1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{}", ip);
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{}", ip);
        }
    }

    #[test]
    fn decodes_shift_jis() {
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode("日本語のページ");
        let html = [b"<meta charset=\"Shift_JIS\">".as_slice(), &bytes].concat();
        assert!(decode(&html, "text/html").contains("日本語のページ"));
    }

    #[test]
    fn html_is_converted_to_text() {
        let text = html_to_text("<html><head><style>p{}</style></head><body><h1>見出し</h1><p>本文です</p></body></html>");
        assert!(text.contains("見出し"));
        assert!(text.contains("本文です"));
        assert!(!text.contains("<p>"));
    }
}
