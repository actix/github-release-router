use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github::Release;

#[derive(Serialize, Deserialize)]
pub struct Message {
    pub payload: Value,
    pub notes: Option<String>,
}

// UTF-16 counting is conservative for supplementary Unicode characters.
fn clip(value: &str, limit: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= limit
        })
        .collect()
}

pub fn render(repository: &str, release: &Release) -> Message {
    let notes = release
        .body
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("No release notes were provided.");
    let long = notes.encode_utf16().count() > 4096;
    let description = if long {
        let excerpt = clip(notes, 3950);
        let excerpt = excerpt
            .rsplit_once('\n')
            .filter(|(before, _)| before.len() > 2000)
            .map_or(excerpt.as_str(), |(before, _)| before);
        format!("{excerpt}\n\n… Full release notes are attached as **release-notes.md**.")
    } else {
        notes.to_owned()
    };
    let name = release
        .name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&release.tag_name);
    let mut payload = serde_json::Map::new();
    payload.insert(
        "allowed_mentions".into(),
        json!({"parse": [], "users": [], "roles": [], "replied_user": false}),
    );
    payload.insert(
        "embeds".into(),
        json!([{"title": clip(name, 256),
            "url": release.html_url, "description": description, "color": 0x5865F2,
            "author": {"name": clip(repository, 256)} }]),
    );
    if long {
        payload.insert(
            "attachments".into(),
            json!([{"id": 0, "filename": "release-notes.md"}]),
        );
    }
    // One embed: title 256 + author 256 + description 4096 <= 6000.
    Message {
        payload: Value::Object(payload),
        notes: long.then(|| notes.to_owned()),
    }
}
