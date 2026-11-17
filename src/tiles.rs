//! Persistent soundpad tiles.
//!
//! Tiles live in `~/.cache/RustMIC/tiles.json` (or `%LOCALAPPDATA%` on
//! Windows). The daemon owns the file; the GUI only sends `AddTile` /
//! `RemoveTile` commands and reads the list back through `AppState`.

use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;
use crate::core::Tile;

fn path() -> std::path::PathBuf {
    config::cache_dir().join("tiles.json")
}

pub fn decode(text: &str) -> Vec<Tile> {
    serde_json::from_str(text).unwrap_or_default()
}

pub fn load() -> Vec<Tile> {
    fs::read_to_string(path())
        .ok()
        .as_deref()
        .map(decode)
        .unwrap_or_default()
}

pub fn save(tiles: &[Tile]) -> io::Result<()> {
    fs::create_dir_all(config::cache_dir())?;
    let text = serde_json::to_string_pretty(tiles)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path(), text)
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

/// Append a tile and return the new list so the caller can refresh `AppState`
/// without waiting for the next poll.
pub fn add(title: &str, query: &str, cover: Option<String>, duration: f64) -> Vec<Tile> {
    let mut tiles = load();
    tiles.push(Tile {
        id: new_id(),
        title: if title.trim().is_empty() {
            query.trim().to_string()
        } else {
            title.trim().to_string()
        },
        query: query.trim().to_string(),
        cover,
        duration,
    });
    if let Err(error) = save(&tiles) {
        crate::logging::log(&format!("cannot save tiles: {error}"));
    }
    tiles
}

pub fn remove(id: &str) -> Vec<Tile> {
    let tiles: Vec<Tile> = load().into_iter().filter(|tile| tile.id != id).collect();
    if let Err(error) = save(&tiles) {
        crate::logging::log(&format!("cannot save tiles: {error}"));
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn bad_tiles_file_is_ignored() {
        assert!(decode("not json").is_empty());
    }

    #[test]
    fn tiles_roundtrip() {
        let text = r#"[{"id":"a","title":"Клип","query":"https://youtu.be/x","cover":null,"duration":210.5}]"#;
        let tiles = decode(text);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].title, "Клип");
        assert_eq!(tiles[0].duration, 210.5);
        let back = serde_json::to_string(&tiles).unwrap();
        assert_eq!(decode(&back).len(), 1);
    }
}
