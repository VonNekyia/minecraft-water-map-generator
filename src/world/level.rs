//! Minimal `level.dat` reader.
//!
//! Only two things are interesting for the analyzer: the world's `DataVersion`
//! (written into the output header so consumers can tell which Minecraft version
//! the data was generated from) and the world name.

use std::io::Read;
use std::path::Path;

use super::nbt::*;

#[derive(Clone, Debug, Default)]
pub struct LevelInfo {
    pub data_version: i32,
    pub level_name: String,
    pub version_name: String,
}

/// Reads `level.dat`. A missing or unreadable file is not fatal - the scan works
/// without it, the header just reports data version 0.
pub fn read(world_dir: &Path) -> LevelInfo {
    let path = world_dir.join("level.dat");
    let Ok(file) = std::fs::File::open(&path) else {
        return LevelInfo::default();
    };
    let mut raw = Vec::new();
    if flate2::read::GzDecoder::new(std::io::BufReader::new(file))
        .read_to_end(&mut raw)
        .is_err()
    {
        // level.dat is normally gzip compressed but may be stored plain.
        let Ok(plain) = std::fs::read(&path) else {
            return LevelInfo::default();
        };
        raw = plain;
    }
    parse(&raw).unwrap_or_default()
}

fn parse(raw: &[u8]) -> Option<LevelInfo> {
    let mut info = LevelInfo::default();
    let mut r = NbtReader::new(raw);
    r.open_root().ok()?;
    while let Some((tag, name)) = r.next_entry().ok()? {
        if tag == TAG_COMPOUND && name == "Data" {
            read_data(&mut r, &mut info).ok()?;
        } else {
            r.skip_payload(tag).ok()?;
        }
    }
    Some(info)
}

fn read_data(r: &mut NbtReader<'_>, info: &mut LevelInfo) -> Result<()> {
    while let Some((tag, name)) = r.next_entry()? {
        match (tag, name) {
            (TAG_INT, "DataVersion") => info.data_version = r.i32()?,
            (TAG_STRING, "LevelName") => info.level_name = r.string()?.to_string(),
            (TAG_COMPOUND, "Version") => {
                while let Some((t2, n2)) = r.next_entry()? {
                    match (t2, n2) {
                        (TAG_STRING, "Name") => info.version_name = r.string()?.to_string(),
                        (TAG_INT, "Id") if info.data_version == 0 => {
                            info.data_version = r.i32()?
                        }
                        _ => r.skip_payload(t2)?,
                    }
                }
            }
            _ => r.skip_payload(tag)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_level_dat() -> Vec<u8> {
        // { "": { "Data": { "DataVersion": 4671, "LevelName": "world" } } }
        let mut v = Vec::new();
        v.push(TAG_COMPOUND);
        v.extend_from_slice(&0u16.to_be_bytes());
        v.push(TAG_COMPOUND);
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice("Data".as_bytes());
        v.push(TAG_INT);
        v.extend_from_slice(&11u16.to_be_bytes());
        v.extend_from_slice("DataVersion".as_bytes());
        v.extend_from_slice(&4671i32.to_be_bytes());
        v.push(TAG_STRING);
        v.extend_from_slice(&9u16.to_be_bytes());
        v.extend_from_slice("LevelName".as_bytes());
        v.extend_from_slice(&5u16.to_be_bytes());
        v.extend_from_slice("world".as_bytes());
        v.push(TAG_END);
        v.push(TAG_END);
        v
    }

    #[test]
    fn parses_data_version_and_name() {
        let info = parse(&build_level_dat()).unwrap();
        assert_eq!(info.data_version, 4671);
        assert_eq!(info.level_name, "world");
    }

    #[test]
    fn missing_file_is_not_fatal() {
        let info = read(Path::new("does-not-exist-anywhere"));
        assert_eq!(info.data_version, 0);
    }
}
