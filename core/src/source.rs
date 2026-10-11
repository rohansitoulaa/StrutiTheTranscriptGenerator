//! What a pasted link points at, decided from the URL alone (PDA IN-2).

use url::Url;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Regular videos, Shorts, live replays and embeds are all one video.
    YouTubeVideo {
        id: String,
    },
    YouTubePlaylist {
        id: String,
    },
    /// Canonical channel URL (`/@handle`, `/channel/UC…`, `/c/…`, `/user/…`).
    YouTubeChannel {
        url: String,
    },
    /// Reels, `/p/` posts and `/tv/` videos, keyed by shortcode.
    InstagramReel {
        shortcode: String,
    },
    InstagramProfile {
        handle: String,
    },
}

impl Source {
    pub fn parse(input: &str) -> Result<Source> {
        let trimmed = input.trim();
        let unsupported = || Error::UnsupportedUrl(trimmed.to_string());

        let with_scheme = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            format!("https://{trimmed}")
        };
        let url = Url::parse(&with_scheme).map_err(|_| unsupported())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(unsupported());
        }
        let host = url.host_str().ok_or_else(unsupported)?.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        let segments: Vec<&str> = url
            .path_segments()
            .map(|s| s.filter(|p| !p.is_empty()).collect())
            .unwrap_or_default();

        let parsed = match host {
            "youtube.com" | "m.youtube.com" | "music.youtube.com" => parse_youtube(&url, &segments),
            "youtu.be" => segments.first().and_then(|id| video_id(id)),
            "instagram.com" | "m.instagram.com" => parse_instagram(&segments),
            _ => None,
        };
        parsed.ok_or_else(unsupported)
    }

    pub fn is_single_video(&self) -> bool {
        matches!(
            self,
            Source::YouTubeVideo { .. } | Source::InstagramReel { .. }
        )
    }

    /// The form handed to yt-dlp: stripped of tracking params and playlist
    /// context, so a `watch?v=…&list=…` link fetches one video.
    pub fn canonical_url(&self) -> String {
        match self {
            Source::YouTubeVideo { id } => format!("https://www.youtube.com/watch?v={id}"),
            Source::YouTubePlaylist { id } => format!("https://www.youtube.com/playlist?list={id}"),
            Source::YouTubeChannel { url } => url.clone(),
            Source::InstagramReel { shortcode } => {
                format!("https://www.instagram.com/reel/{shortcode}/")
            }
            Source::InstagramProfile { handle } => format!("https://www.instagram.com/{handle}/"),
        }
    }

    /// The dedupe key (`platform:id`, PDA DB-2) for a single video, known
    /// from the link alone. Matches `VideoMeta::key` because yt-dlp uses the
    /// same ids (YouTube video id, Instagram shortcode).
    pub fn video_key(&self) -> Option<String> {
        match self {
            Source::YouTubeVideo { id } => Some(format!("youtube:{id}")),
            Source::InstagramReel { shortcode } => Some(format!("instagram:{shortcode}")),
            _ => None,
        }
    }

    /// Short label for errors like "channel links are not supported yet".
    pub fn kind(&self) -> &'static str {
        match self {
            Source::YouTubeVideo { .. } => "video",
            Source::YouTubePlaylist { .. } => "playlist",
            Source::YouTubeChannel { .. } => "channel",
            Source::InstagramReel { .. } => "Instagram reel",
            Source::InstagramProfile { .. } => "Instagram profile",
        }
    }
}

fn parse_youtube(url: &Url, segments: &[&str]) -> Option<Source> {
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    match segments {
        ["watch", ..] => query("v").and_then(|id| video_id(&id)),
        ["shorts" | "live" | "embed", id, ..] => video_id(id),
        ["playlist", ..] => query("list")
            .filter(|id| is_id_chars(id))
            .map(|id| Source::YouTubePlaylist { id }),
        [handle, ..] if handle.starts_with('@') && handle.len() > 1 => {
            Some(Source::YouTubeChannel {
                url: format!("https://www.youtube.com/{handle}"),
            })
        }
        [kind @ ("channel" | "c" | "user"), name, ..] if is_id_chars(name) => {
            Some(Source::YouTubeChannel {
                url: format!("https://www.youtube.com/{kind}/{name}"),
            })
        }
        _ => None,
    }
}

fn parse_instagram(segments: &[&str]) -> Option<Source> {
    let reel = |code: &str| {
        is_id_chars(code).then(|| Source::InstagramReel {
            shortcode: code.to_string(),
        })
    };
    match segments {
        ["reel" | "reels" | "p" | "tv", code, ..] => reel(code),
        [_handle, "reel" | "p", code, ..] => reel(code),
        [handle] | [handle, "reels"] if is_handle(handle) => Some(Source::InstagramProfile {
            handle: handle.to_string(),
        }),
        _ => None,
    }
}

/// YouTube video IDs are exactly 11 URL-safe base64 characters.
fn video_id(id: &str) -> Option<Source> {
    (id.len() == 11 && is_id_chars(id)).then(|| Source::YouTubeVideo { id: id.to_string() })
}

fn is_id_chars(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Instagram top-level paths that are pages, not accounts.
const INSTAGRAM_RESERVED: &[&str] = &[
    "accounts",
    "direct",
    "explore",
    "reels",
    "reel",
    "p",
    "tv",
    "stories",
    "about",
    "legal",
    "developer",
    "web",
    "api",
];

fn is_handle(s: &str) -> bool {
    (1..=30).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        && !INSTAGRAM_RESERVED.contains(&s.to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn video(id: &str) -> Source {
        Source::YouTubeVideo { id: id.into() }
    }

    #[test]
    fn youtube_video_forms() {
        for url in [
            "https://www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://youtube.com/watch?v=jNQXAC9IVRw&t=10s",
            "https://m.youtube.com/watch?v=jNQXAC9IVRw",
            "https://music.youtube.com/watch?v=jNQXAC9IVRw",
            "https://www.youtube.com/watch?v=jNQXAC9IVRw&list=PL123456789",
            "https://youtu.be/jNQXAC9IVRw",
            "https://youtu.be/jNQXAC9IVRw?si=abc",
            "https://www.youtube.com/shorts/jNQXAC9IVRw",
            "https://www.youtube.com/live/jNQXAC9IVRw",
            "https://www.youtube.com/embed/jNQXAC9IVRw",
            "youtube.com/watch?v=jNQXAC9IVRw",
            "  youtu.be/jNQXAC9IVRw  ",
        ] {
            assert_eq!(Source::parse(url).unwrap(), video("jNQXAC9IVRw"), "{url}");
        }
    }

    #[test]
    fn youtube_rejects_bad_video_ids() {
        assert!(Source::parse("https://www.youtube.com/watch?v=short").is_err());
        assert!(Source::parse("https://youtu.be/").is_err());
        assert!(Source::parse("https://www.youtube.com/watch").is_err());
    }

    #[test]
    fn youtube_playlist() {
        assert_eq!(
            Source::parse("https://www.youtube.com/playlist?list=PLabc_DEF-123").unwrap(),
            Source::YouTubePlaylist {
                id: "PLabc_DEF-123".into()
            }
        );
    }

    #[test]
    fn youtube_channels() {
        let cases = [
            (
                "https://www.youtube.com/@kolejain",
                "https://www.youtube.com/@kolejain",
            ),
            (
                "https://www.youtube.com/@kolejain/videos",
                "https://www.youtube.com/@kolejain",
            ),
            (
                "https://youtube.com/@Movies24x7/shorts",
                "https://www.youtube.com/@Movies24x7",
            ),
            (
                "https://www.youtube.com/channel/UC6AhKCzMSkYl-46Ajx55gOA",
                "https://www.youtube.com/channel/UC6AhKCzMSkYl-46Ajx55gOA",
            ),
            (
                "https://www.youtube.com/c/SomeName",
                "https://www.youtube.com/c/SomeName",
            ),
            (
                "https://www.youtube.com/user/SomeName",
                "https://www.youtube.com/user/SomeName",
            ),
            (
                "https://www.youtube.com/channel/UC6AhKCzMSkYl-46Ajx55gOA/videos",
                "https://www.youtube.com/channel/UC6AhKCzMSkYl-46Ajx55gOA",
            ),
            (
                "https://www.youtube.com/c/SomeName/playlists",
                "https://www.youtube.com/c/SomeName",
            ),
            (
                "https://www.youtube.com/user/SomeName/about",
                "https://www.youtube.com/user/SomeName",
            ),
        ];
        for (input, url) in cases {
            assert_eq!(
                Source::parse(input).unwrap(),
                Source::YouTubeChannel { url: url.into() },
                "{input}"
            );
        }
    }

    #[test]
    fn instagram_reels_and_posts() {
        for url in [
            "https://www.instagram.com/reel/DPx0yXkkS5M/",
            "https://instagram.com/reel/DPx0yXkkS5M",
            "https://www.instagram.com/reels/DPx0yXkkS5M/",
            "https://www.instagram.com/p/DPx0yXkkS5M/?igsh=xyz",
            "https://www.instagram.com/tv/DPx0yXkkS5M/",
            "https://www.instagram.com/natgeo/reel/DPx0yXkkS5M/",
        ] {
            assert_eq!(
                Source::parse(url).unwrap(),
                Source::InstagramReel {
                    shortcode: "DPx0yXkkS5M".into()
                },
                "{url}"
            );
        }
    }

    #[test]
    fn instagram_profiles() {
        for url in [
            "https://www.instagram.com/natgeo/",
            "https://www.instagram.com/natgeo",
            "https://www.instagram.com/natgeo/reels/",
            "instagram.com/nat.geo_1",
        ] {
            let handle = if url.contains("nat.geo_1") {
                "nat.geo_1"
            } else {
                "natgeo"
            };
            assert_eq!(
                Source::parse(url).unwrap(),
                Source::InstagramProfile {
                    handle: handle.into()
                },
                "{url}"
            );
        }
    }

    #[test]
    fn instagram_reserved_paths_are_not_profiles() {
        for url in [
            "https://www.instagram.com/explore/",
            "https://www.instagram.com/accounts/login/",
            "https://www.instagram.com/stories/natgeo/123/",
            "https://www.instagram.com/",
            "https://www.instagram.com/reels/",
        ] {
            assert!(Source::parse(url).is_err(), "{url}");
        }
    }

    #[test]
    fn junk_is_rejected_with_the_input_in_the_error() {
        for input in [
            "",
            "hello",
            "https://vimeo.com/123",
            "ftp://youtube.com/watch?v=jNQXAC9IVRw",
        ] {
            match Source::parse(input) {
                Err(Error::UnsupportedUrl(s)) => assert_eq!(s, input.trim()),
                other => panic!("{input:?} gave {other:?}"),
            }
        }
    }

    #[test]
    fn canonical_urls() {
        assert_eq!(
            video("jNQXAC9IVRw").canonical_url(),
            "https://www.youtube.com/watch?v=jNQXAC9IVRw"
        );
        assert_eq!(
            Source::InstagramReel {
                shortcode: "DPx0yXkkS5M".into()
            }
            .canonical_url(),
            "https://www.instagram.com/reel/DPx0yXkkS5M/"
        );
        assert_eq!(
            Source::InstagramProfile {
                handle: "natgeo".into()
            }
            .canonical_url(),
            "https://www.instagram.com/natgeo/"
        );
        assert_eq!(
            Source::YouTubePlaylist { id: "PL1".into() }.canonical_url(),
            "https://www.youtube.com/playlist?list=PL1"
        );
    }

    #[test]
    fn video_keys_match_metadata_keys() {
        assert_eq!(
            video("jNQXAC9IVRw").video_key().as_deref(),
            Some("youtube:jNQXAC9IVRw")
        );
        assert_eq!(
            Source::InstagramReel {
                shortcode: "DboSoFGP5Hw".into()
            }
            .video_key()
            .as_deref(),
            Some("instagram:DboSoFGP5Hw")
        );
        assert_eq!(
            Source::InstagramProfile { handle: "x".into() }.video_key(),
            None
        );
    }

    #[test]
    fn single_video_vs_collections() {
        assert!(video("jNQXAC9IVRw").is_single_video());
        assert!(
            Source::InstagramReel {
                shortcode: "x".into()
            }
            .is_single_video()
        );
        assert!(!Source::YouTubePlaylist { id: "x".into() }.is_single_video());
        assert!(!Source::YouTubeChannel { url: "x".into() }.is_single_video());
        assert!(!Source::InstagramProfile { handle: "x".into() }.is_single_video());
    }
}
