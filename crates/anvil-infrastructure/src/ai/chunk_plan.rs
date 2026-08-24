//! Rozdělení souboru na úseky pro paralelní stahování.
//!
//! Čistá logika bez I/O, aby šla otestovat bez sítě.
//!
//! **Proč vůbec:** HuggingFace omezuje propustnost **na spojení**, ne na
//! klienta. Na gigabitové lince tedy jedno spojení nevytíží ani setinu —
//! naměřeno na 15,6GB modelu: 1 spojení 2,2 MB/s, 4 spojení 9,4 MB/s,
//! 8 spojení 17,3 MB/s. Rozdíl je hodiny proti minutám.
//!
//! **Resume:** `.part` je předalokovaný na plnou velikost a vedle něj leží
//! mapa `.part.map` s jedním bajtem na úsek. Přerušené stahování tak zahodí
//! nejvýš rozdělané úseky, ne celý soubor.

/// Výchozí velikost úseku. Při přerušení se zahodí nejvýš tolik práce na
/// jednoho běžícího workera.
pub const DEFAULT_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Kolik spojení otevřít. Nad osm už HuggingFace nepřidává.
pub const DEFAULT_WORKERS: usize = 8;

/// Pod touhle velikostí se paralelizovat nevyplatí — režie navázání spojení
/// převáží zisk.
pub const MIN_PARALLEL_BYTES: u64 = 2 * DEFAULT_CHUNK_SIZE;

/// Jeden úsek souboru ke stažení.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileChunk {
    pub index: usize,
    pub offset: u64,
    pub length: u64,
}

impl FileChunk {
    /// Poslední bajt úseku — HTTP `Range` je inkluzivní na obou koncích.
    pub fn end_inclusive(&self) -> u64 {
        self.offset + self.length - 1
    }

    /// Hodnota hlavičky `Range` pro tenhle úsek.
    pub fn range_header(&self) -> String {
        format!("bytes={}-{}", self.offset, self.end_inclusive())
    }
}

/// Rozdělí soubor na úseky.
pub fn plan(total_bytes: u64, chunk_size: u64) -> Vec<FileChunk> {
    if total_bytes == 0 {
        return Vec::new();
    }
    let chunk_size = if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        chunk_size
    };

    let count = total_bytes.div_ceil(chunk_size);
    (0..count)
        .map(|i| {
            let offset = i * chunk_size;
            FileChunk {
                index: i as usize,
                offset,
                length: chunk_size.min(total_bytes - offset),
            }
        })
        .collect()
}

/// Kolik spojení použít pro daný počet zbývajících úseků.
pub fn worker_count(chunk_count: usize, max: usize) -> usize {
    if chunk_count == 0 {
        return 0;
    }
    chunk_count.min(max.max(1))
}

/// Mapa hotových úseků — jeden bajt na úsek (0 = chybí, 1 = hotovo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMap {
    done: Vec<bool>,
}

impl ChunkMap {
    pub fn new(chunk_count: usize) -> Self {
        Self {
            done: vec![false; chunk_count],
        }
    }

    /// Načte mapu z bajtů. Vrátí `None`, když neodpovídá očekávanému počtu
    /// úseků — to znamená, že `.part` patří k jinému souboru nebo jinému
    /// dělení, a navazovat na něj by dalo poškozený model.
    pub fn from_bytes(bytes: &[u8], expected_chunks: usize) -> Option<Self> {
        if bytes.len() != expected_chunks {
            return None;
        }
        Some(Self {
            done: bytes.iter().map(|b| *b != 0).collect(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.done.iter().map(|d| u8::from(*d)).collect()
    }

    pub fn mark_done(&mut self, index: usize) {
        if let Some(slot) = self.done.get_mut(index) {
            *slot = true;
        }
    }

    pub fn is_done(&self, index: usize) -> bool {
        self.done.get(index).copied().unwrap_or(false)
    }

    pub fn done_count(&self) -> usize {
        self.done.iter().filter(|d| **d).count()
    }

    pub fn all_done(&self) -> bool {
        self.done.iter().all(|d| *d)
    }

    /// Úseky, které ještě chybí.
    pub fn pending<'a>(&'a self, chunks: &'a [FileChunk]) -> Vec<FileChunk> {
        chunks
            .iter()
            .filter(|c| !self.is_done(c.index))
            .copied()
            .collect()
    }

    /// Kolik bajtů už je hotových — pro průběh po navázání.
    pub fn done_bytes(&self, chunks: &[FileChunk]) -> u64 {
        chunks
            .iter()
            .filter(|c| self.is_done(c.index))
            .map(|c| c.length)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn splits_evenly_and_last_chunk_is_the_remainder() {
        let chunks = plan(150 * MB, 64 * MB);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].length, 64 * MB);
        assert_eq!(chunks[2].offset, 128 * MB);
        assert_eq!(chunks[2].length, 22 * MB);
    }

    #[test]
    fn chunks_cover_the_file_exactly_without_overlap() {
        // Kdyby se úseky překrývaly nebo něco vynechaly, soubor by byl tiše
        // poškozený — velikost by přitom seděla.
        let total = 15_640_000_000;
        let chunks = plan(total, DEFAULT_CHUNK_SIZE);
        assert_eq!(chunks.iter().map(|c| c.length).sum::<u64>(), total);
        for pair in chunks.windows(2) {
            assert_eq!(
                pair[0].end_inclusive() + 1,
                pair[1].offset,
                "mezera nebo překryv mezi úseky"
            );
        }
        assert_eq!(chunks.last().unwrap().end_inclusive(), total - 1);
    }

    #[test]
    fn exact_multiple_has_no_empty_tail_chunk() {
        let chunks = plan(128 * MB, 64 * MB);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.length == 64 * MB));
    }

    #[test]
    fn empty_file_has_no_chunks() {
        assert!(plan(0, 64 * MB).is_empty());
        assert_eq!(worker_count(0, 8), 0);
    }

    #[test]
    fn range_header_is_inclusive_on_both_ends() {
        let chunks = plan(100, 40);
        assert_eq!(chunks[0].range_header(), "bytes=0-39");
        assert_eq!(chunks[1].range_header(), "bytes=40-79");
        assert_eq!(chunks[2].range_header(), "bytes=80-99");
    }

    #[test]
    fn workers_never_exceed_chunks() {
        assert_eq!(worker_count(3, 8), 3);
        assert_eq!(worker_count(100, 8), 8);
        assert_eq!(worker_count(5, 0), 1);
    }

    #[test]
    fn map_round_trips_through_bytes() {
        let mut map = ChunkMap::new(4);
        map.mark_done(1);
        map.mark_done(3);
        let restored = ChunkMap::from_bytes(&map.to_bytes(), 4).expect("mapa");
        assert_eq!(restored, map);
        assert_eq!(restored.done_count(), 2);
        assert!(!restored.all_done());
    }

    #[test]
    fn map_of_wrong_length_is_rejected() {
        // .part po jiném souboru: navázat na něj by dalo poškozený model.
        let map = ChunkMap::new(4);
        assert!(ChunkMap::from_bytes(&map.to_bytes(), 5).is_none());
        assert!(ChunkMap::from_bytes(&[], 4).is_none());
    }

    #[test]
    fn pending_skips_finished_chunks_and_counts_their_bytes() {
        let chunks = plan(150 * MB, 64 * MB);
        let mut map = ChunkMap::new(chunks.len());
        map.mark_done(0);
        map.mark_done(2);

        let pending = map.pending(&chunks);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].index, 1);
        assert_eq!(map.done_bytes(&chunks), 64 * MB + 22 * MB);
    }

    #[test]
    fn fully_finished_map_has_nothing_pending() {
        let chunks = plan(150 * MB, 64 * MB);
        let mut map = ChunkMap::new(chunks.len());
        for c in &chunks {
            map.mark_done(c.index);
        }
        assert!(map.all_done());
        assert!(map.pending(&chunks).is_empty());
        assert_eq!(map.done_bytes(&chunks), 150 * MB);
    }
}
