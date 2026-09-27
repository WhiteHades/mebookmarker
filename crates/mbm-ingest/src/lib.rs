//! source adapters: everything mebookmarker can read from.

pub mod json;
pub mod local;
pub mod markdown_file;
pub mod net;
pub mod x;

pub use json::{parse as parse_json, parse_str as parse_json_str};
pub use local::{parse_netscape, parse_opml, parse_text_document, parse_url_list, read_directory};
pub use markdown_file::{Entry, day_to_unix_ms, parse as parse_markdown_archive};
pub use net::{
    Feed, GithubStars, HackerNews, Reddit, YouTube, parse_feed, parse_github_stars,
    parse_hackernews, parse_reddit, parse_youtube_playlist,
};
pub use x::{Cookies, Folder, Page, X, XClient, parse_bird};
