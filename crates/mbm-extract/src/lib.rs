//! link expansion and content extraction.

pub mod api;
pub mod http;
pub mod links;
pub mod readability;

pub use api::{Browser, Embed, Repo};
pub use http::{Http, Method, Request, Response};
pub use links::{canonical, classify, is_paywalled, links_in};
pub use readability::Article;
