use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{bail, Result};
use url::Url;

pub fn normalize_media_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw)?;
    match url.scheme() {
        "https" | "http" => {}
        _ => bail!("unsupported URL scheme"),
    }

    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if is_blocked_hostname(&host) {
        bail!("blocked host");
    }

    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            bail!("blocked IP range");
        }
    }

    url.set_fragment(None);
    Ok(url.to_string())
}

pub fn normalize_image_url(raw: &str) -> Result<String> {
    normalize_media_url(raw)
}

pub fn normalize_video_url(raw: &str) -> Result<String> {
    normalize_media_url(raw)
}

pub fn is_blocked_hostname(host: &str) -> bool {
    matches!(host, "localhost" | "localhost.localdomain") || host.ends_with(".localhost")
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_ipv4(ip),
        IpAddr::V6(ip) => is_blocked_ipv6(ip),
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.octets()[0] == 0
        || ip.octets()[0] >= 224
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
}

pub fn extract_image_urls(content: &str) -> Vec<String> {
    extract_urls_with_extensions(content, &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif"])
}

pub fn extract_video_urls(content: &str) -> Vec<String> {
    extract_urls_with_extensions(content, &[".mp4", ".webm", ".mov", ".m4v"])
}

fn extract_urls_with_extensions(content: &str, extensions: &[&str]) -> Vec<String> {
    content
        .split_whitespace()
        .filter_map(|part| {
            let trimmed = part.trim_matches(|ch: char| matches!(ch, '"' | '\'' | ')' | '(' | '[' | ']' | '<' | '>' | ','));
            let lower = trimmed.to_ascii_lowercase();
            let looks_like_media = extensions
                .iter()
                .any(|suffix| lower.split('?').next().unwrap_or_default().ends_with(suffix));
            if looks_like_media {
                normalize_media_url(trimmed).ok()
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_local_and_private_hosts() {
        assert!(normalize_image_url("http://localhost/a.png").is_err());
        assert!(normalize_image_url("http://127.0.0.1/a.png").is_err());
        assert!(normalize_image_url("http://10.0.0.5/a.png").is_err());
        assert!(normalize_image_url("http://169.254.10.10/a.png").is_err());
        assert!(normalize_image_url("http://[::1]/a.png").is_err());
    }

    #[test]
    fn normalizes_public_image_url() {
        let normalized = normalize_image_url("https://example.com/a.png#frag").unwrap();
        assert_eq!(normalized, "https://example.com/a.png");
    }

    #[test]
    fn extracts_video_urls() {
        let urls = extract_video_urls("watch https://example.com/a.mp4?x=1 and https://example.com/b.webm");
        assert_eq!(urls, vec!["https://example.com/a.mp4?x=1", "https://example.com/b.webm"]);
    }
}
