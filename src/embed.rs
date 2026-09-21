/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Client-side link embedding.
//!
//! Posts from sites that block scraping — X in particular — render as a bare
//! URL in every Matrix client. Third-party front ends (fxtwitter, vxtwitter)
//! exist to fix that, and this wires them in two ways:
//!
//! - **Rewriting**: a link to `x.com/u/status/1` opens as `fxtwitter.com/u/status/1`.
//!   Works with any front end, because it is just a hostname swap.
//! - **Cards**: when the front end exposes a JSON API, fetch it and draw the
//!   post inline — author, text and a thumbnail.
//!
//! Not every front end can do the second. `vxtwitter` sits behind a
//! Cloudflare challenge and answers a plain HTTP client with 403, so it is
//! rewrite-only; `fxtwitter` serves JSON and can be embedded. The rule's
//! `api` field is what distinguishes them.
//!
//! **This is off by default, and should be.** Fetching a card asks a
//! third-party server for a URL you were sent, which tells that server your
//! IP address and what you are reading. Rewriting alone leaks nothing until
//! you click.

use std::collections::HashMap;

/// A platform and the front end to render it with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EmbedRule {
    /// Shown in settings.
    pub name: String,
    /// Off unless the user turns it on: see the privacy note above.
    pub enabled: bool,
    /// Hostnames this rule claims, including alternative front ends people
    /// post from. `nitter.net` and `fixupx.com` links are still X links.
    #[serde(rename = "aliases", alias = "hosts")]
    pub hosts: Vec<String>,
    pub open_with: String,
    /// Base URL of a JSON API for inline cards. Empty means rewrite only.
    pub api: String,
}

impl EmbedRule {
    /// Matches subdomains too (`mobile.x.com` like `x.com`).
    fn claims(&self, host: &str) -> bool {
        let host = host.trim_start_matches("www.").to_lowercase();
        self.hosts.iter().any(|h| {
            let h = h.to_lowercase();
            host == h || host.ends_with(&format!(".{h}"))
        })
    }
}

/// X defaults, including front-end aliases people post from.
pub fn default_rules() -> Vec<EmbedRule> {
    vec![EmbedRule {
        name: "X / Twitter".into(),
        enabled: false,
        hosts: vec![
            "x.com".into(),
            "twitter.com".into(),
            "vxtwitter.com".into(),
            "fxtwitter.com".into(),
            "fixupx.com".into(),
            "fixvx.com".into(),
            "nitter.net".into(),
        ],
        open_with: "fxtwitter.com".into(),
        // fxtwitter answers plain HTTP clients; vxtwitter does not.
        api: "https://api.fxtwitter.com".into(),
    }]
}

#[derive(Debug, Clone)]
pub struct Embed {
    pub author: String,
    pub handle: String,
    /// Author avatar.
    pub avatar: Option<String>,
    pub text: String,
    pub image: Option<String>,
    pub link: String,
}

/// Swap host, keep path/query.
pub fn rewrite(rules: &[EmbedRule], url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let rule = rules.iter().find(|r| r.enabled && r.claims(host))?;
    if rule.open_with.is_empty() || rule.open_with.eq_ignore_ascii_case(host) {
        return None;
    }
    let mut out = parsed.clone();
    out.set_host(Some(&rule.open_with)).ok()?;
    Some(out.to_string())
}

pub fn card_rule<'a>(rules: &'a [EmbedRule], url: &str) -> Option<&'a EmbedRule> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    rules
        .iter()
        .find(|r| r.enabled && !r.api.is_empty() && r.claims(host))
}

/// Path carries across unchanged; front ends mirror `/user/status/id`.
pub async fn fetch(rule: &EmbedRule, url: &str) -> Result<Embed, String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let endpoint = format!("{}{}", rule.api.trim_end_matches('/'), parsed.path());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        // Some front ends refuse a request with no agent.
        .user_agent("thrace")
        .build()
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = client
        .get(&endpoint)
        .send()
        .await
        .map_err(|e| format!("{e}"))?
        .json()
        .await
        .map_err(|e| format!("{e}"))?;

    parse(&body, url)
}

/// Split from the request so field extraction can be tested without network.
pub fn parse(body: &serde_json::Value, url: &str) -> Result<Embed, String> {
    // fxtwitter nests everything under `tweet`; tolerate a flat shape too so
    // a differently-shaped front end still works.
    let post = body.get("tweet").unwrap_or(body);
    let author = post.get("author");
    let text = post
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    if text.is_empty() && author.is_none() {
        return Err("no post in the response".into());
    }
    Ok(Embed {
        author: author
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned(),
        handle: author
            .and_then(|a| a.get("screen_name"))
            .and_then(|v| v.as_str())
            .map(|s| format!("@{s}"))
            .unwrap_or_default(),
        avatar: author
            .and_then(|a| a.get("avatar_url"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        text,
        image: post
            .get("media")
            .and_then(|m| m.get("all"))
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|m| {
                // Video has a thumbnail; a photo is its own URL.
                m.get("thumbnail_url")
                    .or_else(|| m.get("url"))
                    .and_then(|v| v.as_str())
            })
            .map(str::to_owned),
        link: post
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or(url)
            .to_owned(),
    })
}

/// Ordinary https URL, not mxc: outside the Matrix media cache. Oversized
/// images downscaled, not rejected.
pub async fn fetch_image(url: &str) -> Result<egui::ColorImage, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("thrace")
        .build()
        .map_err(|e| e.to_string())?;
    let bytes = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{e}"))?
        .bytes()
        .await
        .map_err(|e| format!("{e}"))?;
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        crate::media_cache::decode_image(&bytes, crate::media_cache::THUMB_MAX_PIXELS)
    })
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "undecodable image".to_owned())
}

#[derive(Default)]
pub struct EmbedCache {
    entries: HashMap<String, Option<Embed>>,
    /// URLs already requested, so a failure is not retried every frame.
    asked: std::collections::HashSet<String>,
}

impl EmbedCache {
    pub fn get(&self, url: &str) -> Option<&Embed> {
        self.entries.get(url)?.as_ref()
    }

    pub fn claim(&mut self, url: &str) -> bool {
        self.asked.insert(url.to_owned())
    }

    pub fn insert(&mut self, url: String, embed: Option<Embed>) {
        self.entries.insert(url, embed);
    }

    /// Rules changed: a URL's card depends on which front end produced it.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.asked.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<EmbedRule> {
        let mut r = default_rules();
        r[0].enabled = true;
        r
    }

    #[test]
    fn rewrites_known_hosts_and_their_aliases() {
        let r = rules();
        assert_eq!(
            rewrite(&r, "https://x.com/foo/status/123").as_deref(),
            Some("https://fxtwitter.com/foo/status/123")
        );
        assert_eq!(
            rewrite(&r, "https://twitter.com/foo/status/123").as_deref(),
            Some("https://fxtwitter.com/foo/status/123")
        );
        assert_eq!(
            rewrite(&r, "https://nitter.net/foo/status/123").as_deref(),
            Some("https://fxtwitter.com/foo/status/123")
        );
        assert_eq!(
            rewrite(&r, "https://mobile.x.com/foo/status/123").as_deref(),
            Some("https://fxtwitter.com/foo/status/123")
        );
        assert_eq!(
            rewrite(&r, "https://x.com/a/status/1?s=20").as_deref(),
            Some("https://fxtwitter.com/a/status/1?s=20")
        );
    }

    #[test]
    fn leaves_everything_else_alone() {
        let r = rules();
        assert!(rewrite(&r, "https://example.com/x.com").is_none());
        assert!(rewrite(&r, "not a url").is_none());
        // A host that merely ends with the same letters is not a match.
        assert!(rewrite(&r, "https://notx.com/a").is_none());
        assert!(rewrite(&r, "https://fxtwitter.com/a/status/1").is_none());
    }

    #[test]
    fn disabled_rules_do_nothing() {
        // Off by default, because fetching a card tells a third party what
        // you are reading.
        let off = default_rules();
        assert!(!off[0].enabled, "embedding must be opt-in");
        assert!(rewrite(&off, "https://x.com/a/status/1").is_none());
        assert!(card_rule(&off, "https://x.com/a/status/1").is_none());
    }

    #[test]
    fn only_rules_with_an_api_produce_cards() {
        let mut r = rules();
        assert!(card_rule(&r, "https://x.com/a/status/1").is_some());
        // vxtwitter is rewrite-only: it answers plain HTTP clients with 403.
        r[0].api.clear();
        assert!(card_rule(&r, "https://x.com/a/status/1").is_none());
        assert!(rewrite(&r, "https://x.com/a/status/1").is_some());
    }

    #[test]
    fn parses_a_real_fxtwitter_response() {
        // Trimmed from an actual api.fxtwitter.com reply, so the field paths
        // are checked against the shape the service really returns.
        let body: serde_json::Value = serde_json::from_str(
            r#"{
              "code": 200,
              "message": "OK",
              "tweet": {
                "url": "https://x.com/XDevelopers/status/1460323737035677698",
                "text": "Introducing a new era",
                "author": { "name": "Developers", "screen_name": "XDevelopers" },
                "media": { "all": [ {
                    "url": "https://video.twimg.com/a.mp4",
                    "thumbnail_url": "https://pbs.twimg.com/b.jpg"
                } ] }
              }
            }"#,
        )
        .expect("fixture parses");

        let embed = parse(&body, "https://x.com/whatever").expect("card");
        assert_eq!(embed.author, "Developers");
        assert_eq!(embed.handle, "@XDevelopers");
        assert_eq!(embed.text, "Introducing a new era");
        assert_eq!(embed.image.as_deref(), Some("https://pbs.twimg.com/b.jpg"));
        assert!(embed.link.contains("XDevelopers/status"));
    }

    #[test]
    fn a_photo_post_uses_its_own_url() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"tweet":{"text":"hi","author":{"name":"A","screen_name":"a"},
                 "media":{"all":[{"url":"https://pbs.twimg.com/photo.jpg"}]}}}"#,
        )
        .unwrap();
        let embed = parse(&body, "https://x.com/a/status/1").unwrap();
        assert_eq!(
            embed.image.as_deref(),
            Some("https://pbs.twimg.com/photo.jpg")
        );
    }

    #[test]
    fn an_empty_response_is_an_error_not_a_blank_card() {
        let body: serde_json::Value = serde_json::json!({"code": 404});
        assert!(parse(&body, "https://x.com/a/status/1").is_err());
    }

    #[test]
    fn host_lists_round_trip_through_a_comma_separated_field() {
        // The settings field is comma separated; parsing it must survive
        // spaces, stray separators and mixed case without losing hosts.
        let typed = " X.com , twitter.com,, NITTER.net ,";
        let parsed: Vec<String> = typed
            .split(',')
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        assert_eq!(parsed, vec!["x.com", "twitter.com", "nitter.net"]);

        let mut rule = default_rules().remove(0);
        rule.enabled = true;
        rule.hosts = parsed.clone();
        assert!(rewrite(&[rule], "https://nitter.net/a/status/1").is_some());

        assert_eq!(parsed.join(", "), "x.com, twitter.com, nitter.net");
    }

    #[test]
    fn parses_the_author_avatar() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"tweet":{"text":"hi","author":{"name":"A","screen_name":"a",
                 "avatar_url":"https://pbs.twimg.com/pic.jpg"}}}"#,
        )
        .unwrap();
        let embed = parse(&body, "https://x.com/a/status/1").unwrap();
        assert_eq!(
            embed.avatar.as_deref(),
            Some("https://pbs.twimg.com/pic.jpg")
        );
    }

    #[test]
    fn cache_asks_once_even_when_the_fetch_fails() {
        let mut cache = EmbedCache::default();
        assert!(cache.claim("https://x.com/a/status/1"));
        assert!(!cache.claim("https://x.com/a/status/1"), "must not re-ask");
        cache.insert("https://x.com/a/status/1".into(), None);
        assert!(cache.get("https://x.com/a/status/1").is_none());
    }
}
