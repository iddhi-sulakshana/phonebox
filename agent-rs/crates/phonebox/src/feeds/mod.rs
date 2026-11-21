//! `FeedMonitor.cs`: headlines from the RSS and Atom feeds in the settings, for the Feeds widget.

mod html_entities;
pub mod parser;

use serde::ser::{Serialize, SerializeStruct, Serializer};

/// One headline. The link stays on this computer: the phone opens an item by its id.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedItem {
    pub id: String,
    pub title: String,
    pub source: String,
    pub published: i64,
    pub link: Option<String>,
}

impl FeedItem {
    pub fn openable(&self) -> bool {
        self.link.is_some()
    }
}

impl Serialize for FeedItem {
    /// `{"id","title","source","published","openable"}`: the link is never sent.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut o = s.serialize_struct("FeedItem", 5)?;
        o.serialize_field("id", &self.id)?;
        o.serialize_field("title", &self.title)?;
        o.serialize_field("source", &self.source)?;
        o.serialize_field("published", &self.published)?;
        o.serialize_field("openable", &self.openable())?;
        o.end()
    }
}
