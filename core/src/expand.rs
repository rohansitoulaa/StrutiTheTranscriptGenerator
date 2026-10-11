//! Channels, playlists and profiles → the single videos in them (PDA IN-4).
//!
//! yt-dlp lists a collection flat (`--flat-playlist -J`): one request per
//! page of entries, no per-video metadata. A channel is its Videos and Shorts
//! tabs; live streams are left out.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::source::Source;
use crate::ytdlp::Fetcher;
use crate::{Error, Result};

/// One flat listing as yt-dlp reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// The playlist's (or tab's) own title.
    pub title: Option<String>,
    /// The channel or uploader.
    pub channel: Option<String>,
    pub entries: Vec<ListedVideo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedVideo {
    pub url: String,
    pub title: Option<String>,
}

/// A pasted collection, expanded and de-duplicated, in listing order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Collection {
    pub title: String,
    pub videos: Vec<Video>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Video {
    /// The dedupe key, as for a pasted single video.
    pub key: String,
    pub url: String,
    pub title: Option<String>,
}

impl Listing {
    pub fn from_ytdlp_json(json: &str) -> Result<Listing> {
        #[derive(Deserialize)]
        struct Raw {
            title: Option<String>,
            channel: Option<String>,
            uploader: Option<String>,
            #[serde(default)]
            entries: Vec<Option<RawEntry>>,
        }
        #[derive(Deserialize)]
        struct RawEntry {
            url: Option<String>,
            id: Option<String>,
            title: Option<String>,
            ie_key: Option<String>,
            live_status: Option<String>,
        }

        let raw: Raw = serde_json::from_str(json).map_err(|e| Error::Parse {
            what: "yt-dlp listing",
            reason: e.to_string(),
        })?;
        let entries = raw
            .entries
            .into_iter()
            .flatten()
            .filter(|e| {
                !matches!(
                    e.live_status.as_deref(),
                    Some("is_live" | "is_upcoming" | "post_live")
                )
            })
            .filter_map(|e| {
                let url = e.url.or_else(|| {
                    (e.ie_key.as_deref() == Some("Youtube"))
                        .then(|| {
                            e.id.map(|id| format!("https://www.youtube.com/watch?v={id}"))
                        })
                        .flatten()
                })?;
                Some(ListedVideo {
                    url,
                    title: e.title,
                })
            })
            .collect();
        Ok(Listing {
            title: raw.title,
            channel: raw.channel.or(raw.uploader),
            entries,
        })
    }
}

/// What yt-dlp is asked to list for a collection; empty for single videos.
pub fn listing_urls(source: &Source) -> Vec<String> {
    match source {
        Source::YouTubeChannel { url } => vec![format!("{url}/videos"), format!("{url}/shorts")],
        Source::YouTubePlaylist { .. } | Source::InstagramProfile { .. } => {
            vec![source.canonical_url()]
        }
        Source::YouTubeVideo { .. } | Source::InstagramReel { .. } => Vec::new(),
    }
}

/// Lists every video in a collection, at most `limit` of them.
pub async fn expand<F: Fetcher>(
    fetcher: &F,
    source: &Source,
    limit: Option<usize>,
) -> Result<Collection> {
    let mut title = None;
    let mut videos = Vec::new();
    let mut seen = HashSet::new();
    for url in listing_urls(source) {
        let remaining = limit.map(|l| l.saturating_sub(videos.len()));
        if remaining == Some(0) {
            break;
        }
        let listing = match fetcher.list(&url, remaining).await {
            Ok(listing) => listing,
            // "This channel does not have a shorts tab": nothing there.
            Err(Error::ToolFailed { stderr, .. })
                if stderr.contains("does not have a") && stderr.contains(" tab") =>
            {
                tracing::debug!(%url, "no such tab; treating it as empty");
                continue;
            }
            // Throttling stays recognisable, so Instagram jobs pause.
            Err(e @ Error::InstagramBlocked(_)) => return Err(e),
            Err(e @ Error::ToolFailed { .. })
                if matches!(source, Source::InstagramProfile { .. }) =>
            {
                return Err(Error::State(format!(
                    "could not list the profile (yt-dlp's Instagram profile support is \
                     marked broken upstream; an update may fix it, or paste the reel links): {e}"
                )));
            }
            Err(e) => return Err(e),
        };
        // A playlist goes by its own name; a channel tab ("X - Videos") or
        // a profile by its owner.
        let name = match source {
            Source::YouTubePlaylist { .. } => listing.title.or(listing.channel),
            _ => listing.channel.or(listing.title),
        };
        title = title.or(name);
        for entry in listing.entries {
            if limit.is_some_and(|l| videos.len() >= l) {
                break;
            }
            let video = Source::parse(&entry.url)
                .ok()
                .and_then(|s| Some((s.video_key()?, s.canonical_url())));
            match video {
                Some((key, url)) => {
                    if seen.insert(key.clone()) {
                        videos.push(Video {
                            key,
                            url,
                            title: entry.title,
                        });
                    }
                }
                None => {
                    tracing::warn!(url = %entry.url, "skipping a listed entry that isn't a video")
                }
            }
        }
    }
    Ok(Collection {
        title: title.unwrap_or_else(|| source.canonical_url()),
        videos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{CaptionTrack, VideoMeta};
    use crate::transcript::Segment;
    use pretty_assertions::assert_eq;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type Answers = Vec<(&'static str, fn() -> Result<Listing>)>;

    /// Answers `list` from a table of URL → result; records each call.
    struct Lister {
        answers: Answers,
        calls: Mutex<Vec<(String, Option<usize>)>>,
    }

    impl Fetcher for Lister {
        async fn metadata(&self, _url: &str) -> Result<VideoMeta> {
            unreachable!()
        }
        async fn captions(&self, _: &str, _: &CaptionTrack, _: &Path) -> Result<Vec<Segment>> {
            unreachable!()
        }
        async fn audio(&self, _: &str, _: &Path) -> Result<PathBuf> {
            unreachable!()
        }
        async fn list(&self, url: &str, limit: Option<usize>) -> Result<Listing> {
            self.calls.lock().unwrap().push((url.to_string(), limit));
            let answer = self.answers.iter().find(|(u, _)| *u == url).unwrap().1;
            answer()
        }
    }

    fn lister(answers: Answers) -> Lister {
        Lister {
            answers,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn listed(channel: &str, urls: &[&str]) -> Listing {
        Listing {
            title: Some(format!("{channel} - Videos")),
            channel: Some(channel.into()),
            entries: urls
                .iter()
                .map(|u| ListedVideo {
                    url: u.to_string(),
                    title: Some(format!("t {u}")),
                })
                .collect(),
        }
    }

    #[test]
    fn parses_a_flat_channel_tab() {
        let json = r#"{"id": "UC1", "title": "Kole Jain - Videos", "channel": "Kole Jain",
            "_type": "playlist", "entries": [
              {"_type": "url", "ie_key": "Youtube", "id": "neE6wOuBIP8",
               "url": "https://www.youtube.com/watch?v=neE6wOuBIP8", "title": "One", "live_status": null},
              {"_type": "url", "ie_key": "Youtube", "id": "liveLIVE001", "title": "Live now",
               "url": "https://www.youtube.com/watch?v=liveLIVE001", "live_status": "is_live"},
              {"_type": "url", "ie_key": "Youtube", "id": "RCneB_MQ7qs", "title": "No url"},
              null
            ]}"#;
        let listing = Listing::from_ytdlp_json(json).unwrap();
        assert_eq!(listing.title.as_deref(), Some("Kole Jain - Videos"));
        assert_eq!(listing.channel.as_deref(), Some("Kole Jain"));
        assert_eq!(
            listing.entries,
            [
                ListedVideo {
                    url: "https://www.youtube.com/watch?v=neE6wOuBIP8".into(),
                    title: Some("One".into())
                },
                ListedVideo {
                    url: "https://www.youtube.com/watch?v=RCneB_MQ7qs".into(),
                    title: Some("No url".into())
                },
            ]
        );
        assert!(Listing::from_ytdlp_json("not json").is_err());
    }

    #[test]
    fn what_gets_listed_for_each_kind_of_link() {
        let urls = |link: &str| listing_urls(&Source::parse(link).unwrap());
        assert_eq!(
            urls("https://www.youtube.com/@kolejain/featured"),
            [
                "https://www.youtube.com/@kolejain/videos",
                "https://www.youtube.com/@kolejain/shorts"
            ]
        );
        assert_eq!(
            urls("https://www.youtube.com/playlist?list=PL123"),
            ["https://www.youtube.com/playlist?list=PL123"]
        );
        assert_eq!(
            urls("https://www.instagram.com/synsation_/"),
            ["https://www.instagram.com/synsation_/"]
        );
        assert!(urls("https://youtu.be/neE6wOuBIP8").is_empty());
    }

    #[tokio::test]
    async fn a_channel_is_its_videos_and_shorts_deduplicated() {
        let f = lister(vec![
            ("https://www.youtube.com/@k/videos", || {
                Ok(listed(
                    "K",
                    &[
                        "https://www.youtube.com/watch?v=AAAAAAAAAAA",
                        "https://www.youtube.com/watch?v=BBBBBBBBBBB",
                    ],
                ))
            }),
            ("https://www.youtube.com/@k/shorts", || {
                Ok(listed(
                    "K",
                    &[
                        "https://www.youtube.com/shorts/CCCCCCCCCCC",
                        "https://www.youtube.com/watch?v=AAAAAAAAAAA",
                        "https://example.com/not-a-video",
                    ],
                ))
            }),
        ]);
        let c = expand(
            &f,
            &Source::parse("https://www.youtube.com/@k").unwrap(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(c.title, "K");
        let keys: Vec<&str> = c.videos.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "youtube:AAAAAAAAAAA",
                "youtube:BBBBBBBBBBB",
                "youtube:CCCCCCCCCCC"
            ]
        );
        assert_eq!(
            c.videos[2].url,
            "https://www.youtube.com/watch?v=CCCCCCCCCCC"
        );
    }

    #[tokio::test]
    async fn a_missing_shorts_tab_is_just_empty_and_the_limit_caps_the_total() {
        let f = lister(vec![
            ("https://www.youtube.com/@k/videos", || {
                Ok(listed(
                    "K",
                    &[
                        "https://www.youtube.com/watch?v=AAAAAAAAAAA",
                        "https://www.youtube.com/watch?v=BBBBBBBBBBB",
                    ],
                ))
            }),
            ("https://www.youtube.com/@k/shorts", || {
                Err(Error::ToolFailed {
                    tool: "yt-dlp",
                    status: "exit status: 1".into(),
                    stderr: "ERROR: [youtube:tab] @k: This channel does not have a shorts tab"
                        .into(),
                })
            }),
        ]);
        let channel = Source::parse("https://www.youtube.com/@k").unwrap();
        assert_eq!(expand(&f, &channel, None).await.unwrap().videos.len(), 2);

        let capped = expand(&f, &channel, Some(1)).await.unwrap();
        assert_eq!(capped.videos.len(), 1);
        let calls = f.calls.lock().unwrap().clone();
        // The cap is passed down, and a full cap skips the Shorts listing.
        assert_eq!(
            calls.last().unwrap(),
            &("https://www.youtube.com/@k/videos".to_string(), Some(1))
        );
        assert_eq!(calls.len(), 3);
    }

    #[tokio::test]
    async fn a_profile_that_cant_be_listed_says_why() {
        let f = lister(vec![("https://www.instagram.com/someone/", || {
            Err(Error::ToolFailed {
                tool: "yt-dlp",
                status: "exit status: 1".into(),
                stderr: "ERROR: [instagram:user] someone: Unable to extract data".into(),
            })
        })]);
        let err = expand(
            &f,
            &Source::parse("https://www.instagram.com/someone/").unwrap(),
            None,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("paste the reel links") && msg.contains("Unable to extract"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn a_playlist_goes_by_its_own_name() {
        let f = lister(vec![("https://www.youtube.com/playlist?list=PLx", || {
            Ok(Listing {
                title: Some("Motion Design".into()),
                channel: Some("Some Channel".into()),
                entries: Vec::new(),
            })
        })]);
        let c = expand(
            &f,
            &Source::parse("https://www.youtube.com/playlist?list=PLx").unwrap(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(c.title, "Motion Design");
    }

    #[tokio::test]
    async fn a_throttled_profile_listing_stays_a_block() {
        let f = lister(vec![("https://www.instagram.com/someone/", || {
            Err(Error::InstagramBlocked("429".into()))
        })]);
        let err = expand(
            &f,
            &Source::parse("https://www.instagram.com/someone/").unwrap(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::InstagramBlocked(_)), "{err:?}");
    }

    #[tokio::test]
    async fn other_listing_failures_pass_through() {
        let f = lister(vec![("https://www.youtube.com/playlist?list=PLx", || {
            Err(Error::Timeout {
                tool: "yt-dlp",
                secs: 600,
            })
        })]);
        let err = expand(
            &f,
            &Source::parse("https://www.youtube.com/playlist?list=PLx").unwrap(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    }
}
