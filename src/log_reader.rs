use std::{
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::PathBuf,
};

use regex::RegexSet;

pub struct Reader {
    path: PathBuf,
    patterns: RegexSet,
    byte_offset: u64,
}

impl Reader {
    pub fn read_latest(&mut self, mut f: impl FnMut(&str)) -> Result<(), std::io::Error> {
        let file = File::open(&self.path)?;
        let new_byte_offset = file.metadata()?.len();

        if new_byte_offset < self.byte_offset {
            self.byte_offset = 0;
        }

        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.byte_offset))?;

        let lines = reader.lines();
        lines.for_each(|line| {
            if let Ok(line) = line
                && self.patterns.matches(&line).matched_any()
            {
                f(&line)
            }
        });

        self.byte_offset = new_byte_offset;

        Ok(())
    }
}

pub fn build(path: impl Into<PathBuf>, patterns: RegexSet) -> Result<Reader, std::io::Error> {
    let mut reader = Reader {
        path: path.into(),
        patterns,
        byte_offset: 0,
    };

    reader.read_latest(|_| {})?;
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path() -> PathBuf {
        std::env::temp_dir().join(format!("exile-log-api-test-{}-{}.txt", std::process::id(), {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            N.fetch_add(1, Ordering::Relaxed)
        }))
    }

    #[test]
    fn truncate_resets_offset() {
        let path = unique_path();
        std::fs::write(&path, "2026/01/01 Generating level 1 area \"x\" with seed 1\n2026/01/01 unrelated client startup noise\n").unwrap();
        let patterns = RegexSet::new([r"Generating level \d+ area"]).unwrap();
        let mut reader = build(&path, patterns).unwrap();
        let mut got: Vec<String> = Vec::new();
        reader.read_latest(|line| got.push(line.to_string())).unwrap();
        assert!(got.is_empty(), "drained lines must not resend: {got:?}");
        std::fs::write(&path, "2026/01/01 Generating level 2 area \"y\" with seed 2\n").unwrap();
        reader.read_latest(|line| got.push(line.to_string())).unwrap();
        assert!(
            got.iter().any(|line| line.contains("area \"y\"")),
            "truncated log must deliver new lines: {got:?}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
