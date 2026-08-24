//! Stahování GGUF modelů — paralelně, s navázáním po přerušení.
//!
//! **Proč paralelně:** HuggingFace omezuje propustnost na spojení, ne na
//! klienta. Jedním proudem tekl 15,6GB model rychlostí ~0,6 MB/s (7,7 hodiny)
//! i na gigabitové lince; osmi spojeními je to řádově jinde. Rozdělení na
//! úseky a mapu hotových kusů řeší [`super::chunk_plan`].
//!
//! **HTTP/1.1 je tu nutnost, ne preference.** Přes HTTP/2 klient všechny
//! požadavky namultiplexuje do jednoho TCP spojení, takže by limit na spojení
//! platil dál a paralelismus by nepřinesl nic.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{
    fs::OpenOptions,
    io::{AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use anvil_domain::model::ModelSpec;

use super::chunk_plan::{self, ChunkMap, FileChunk};

/// Všechno, co downloader o stahovaném souboru potřebuje vědět.
///
/// Záměrně nebere `ModelSpec` přímo: stahování je čistě mechanická operace
/// nad URL a názvem souboru a nemá důvod znát role, kvantizaci ani šablonu
/// promptu. Díky tomu jde downloader testovat i proti souboru, který
/// v katalogu vůbec není.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTarget {
    /// Identifikace do logu a průběhu.
    pub id: String,
    pub url: String,
    pub filename: String,
    /// Očekávaná velikost. Je to jen odhad z katalogu — závazné je až
    /// `Content-Length`, proti kterému se po stažení ověří.
    pub size_bytes: u64,
}

impl From<&ModelSpec> for DownloadTarget {
    fn from(spec: &ModelSpec) -> Self {
        Self {
            id: spec.id.to_string(),
            url: spec.download_url(),
            filename: spec.local_filename().to_string(),
            size_bytes: spec.size_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub model_id: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bajty za vteřinu (klouzavý průměr).
    pub speed_bps: u64,
    pub status: DownloadStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadStatus {
    Starting,
    InProgress,
    Completed,
    Failed { message: String },
    Cancelled,
}

/// GGUF soubor nalezený na disku.
///
/// Není to doménová `InstalledModel` — tohle je jen nález na disku, který
/// s katalogem ještě nikdo nespároval. Párování dělá `ModelProvisioner`,
/// aby downloader nemusel katalog vůbec znát.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModelFile {
    pub filename: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Clone)]
pub struct ModelDownloader {
    client: reqwest::Client,
    models_dir: PathBuf,
    /// Volitelný HuggingFace token pro stahování gated modelů. Přidá se
    /// jako `Authorization: Bearer <token>` hlavičku.
    hf_token: Option<String>,
}

impl ModelDownloader {
    pub fn new(models_dir: PathBuf) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60 * 60 * 6)) // 6h celkový timeout
            .connect_timeout(Duration::from_secs(30))
            // Bez tohohle je paralelní stahování k ničemu: přes HTTP/2 by se
            // všech osm požadavků namultiplexovalo do jednoho TCP spojení a
            // limit HuggingFacu na spojení by platil dál.
            .http1_only()
            .pool_max_idle_per_host(chunk_plan::DEFAULT_WORKERS)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            models_dir,
            hf_token: None,
        }
    }

    pub fn set_hf_token(&mut self, token: Option<String>) {
        self.hf_token = token.filter(|t| !t.trim().is_empty());
    }

    pub fn has_hf_token(&self) -> bool {
        self.hf_token.is_some()
    }

    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    pub fn list_installed(&self) -> Vec<LocalModelFile> {
        let Ok(entries) = std::fs::read_dir(&self.models_dir) else {
            return vec![];
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !filename.ends_with(".gguf") {
                continue;
            }
            let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(LocalModelFile {
                filename: filename.to_string(),
                path: path.to_string_lossy().to_string(),
                size_bytes,
            });
        }
        out.sort_by(|a, b| a.filename.cmp(&b.filename));
        out
    }

    pub fn delete(&self, filename: &str) -> Result<()> {
        if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
            return Err(anyhow!("Nepovolené jméno souboru: {filename}"));
        }
        let path = self.models_dir.join(filename);
        if !path.is_file() {
            return Err(anyhow!("Soubor neexistuje: {}", path.display()));
        }
        std::fs::remove_file(&path).with_context(|| format!("Nelze smazat {}", path.display()))?;
        Ok(())
    }

    /// Stáhne model. `on_progress` se volá zhruba každých 250 ms a po dokončení.
    ///
    /// Když server umí `Range` a soubor je dost velký, stahuje se osmi
    /// spojeními; jinak jedním proudem.
    pub async fn download<F>(
        &self,
        model: &DownloadTarget,
        cancel: CancellationToken,
        on_progress: Arc<F>,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        tokio::fs::create_dir_all(&self.models_dir).await.ok();

        match self.probe_ranges(&model.url).await {
            Ok(Some(total)) if total >= chunk_plan::MIN_PARALLEL_BYTES => {
                tracing::info!(
                    model = %model.id,
                    size_mb = total / 1_048_576,
                    "Server umí Range — stahuji paralelně"
                );
                self.download_parallel(model, total, cancel, on_progress)
                    .await
            }
            Ok(_) => {
                tracing::info!(
                    model = %model.id,
                    "Malý soubor nebo bez podpory Range — stahuji jedním proudem"
                );
                self.download_single(model, cancel, on_progress).await
            }
            Err(e) => {
                // Nedostupnost serveru se projeví znovu v samotném stahování,
                // kde je hlášení chyby pro uživatele srozumitelnější.
                tracing::warn!(error = %e, "Zjištění podpory Range selhalo — jedním proudem");
                self.download_single(model, cancel, on_progress).await
            }
        }
    }

    /// Zjistí, jestli server umí `Range`, a jak je soubor velký.
    ///
    /// Ptáme se ranged GETem na jediný bajt, ne HEADem: HuggingFace posílá na
    /// CDN přes přesměrování a HEAD na něm nemusí vrátit totéž co GET.
    /// `Some(total)` = umí a známe velikost, `None` = neumí.
    async fn probe_ranges(&self, url: &str) -> Result<Option<u64>> {
        let mut req = self
            .client
            .get(url)
            .header(reqwest::header::RANGE, "bytes=0-0");
        if let Some(token) = &self.hf_token {
            req = req.bearer_auth(token);
        }

        let response = req.send().await.context("probe request selhal")?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Ok(None);
        }

        let total = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_total);

        Ok(total)
    }

    /// Stažení jedním proudem — záloha pro servery bez `Range` a pro malé
    /// soubory.
    async fn download_single<F>(
        &self,
        model: &DownloadTarget,
        cancel: CancellationToken,
        on_progress: Arc<F>,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        tokio::fs::create_dir_all(&self.models_dir).await.ok();
        let target = self.models_dir.join(&model.filename);
        let temp = self.models_dir.join(format!("{}.part", &model.filename));

        on_progress(DownloadProgress {
            model_id: model.id.clone(),
            downloaded: 0,
            total: Some(model.size_bytes),
            speed_bps: 0,
            status: DownloadStatus::Starting,
        });

        // Resume — pokud .part existuje, navážeme na něj.
        let mut downloaded: u64 = 0;
        let mut req = self.client.get(&model.url);
        if let Some(token) = &self.hf_token {
            req = req.bearer_auth(token);
        }
        if temp.exists() {
            let existing = tokio::fs::metadata(&temp).await?.len();
            if existing > 0 {
                downloaded = existing;
                req = req.header(reqwest::header::RANGE, format!("bytes={existing}-"));
                tracing::info!(model = %model.id, %existing, "Pokračuji v downloadu");
            }
        }

        let response = req
            .send()
            .await
            .with_context(|| format!("HTTP request failed for {}", model.url))?;

        if !response.status().is_success()
            && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        {
            return Err(anyhow!(
                "Server vrátil HTTP {}: {}",
                response.status(),
                model.url
            ));
        }

        let content_length = response.content_length();
        // Autoritativní očekávaná velikost (od serveru) — pokud ji známe,
        // po stažení ověříme úplnost. model.size_bytes je jen odhad z katalogu.
        let initial_downloaded = downloaded;
        let expected_final = content_length.map(|cl| cl + initial_downloaded);
        let total = expected_final.or(Some(model.size_bytes));

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(downloaded > 0)
            .truncate(downloaded == 0)
            .open(&temp)
            .await
            .with_context(|| format!("Nelze otevřít {}", temp.display()))?;

        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;

        let start = Instant::now();
        let mut last_report = Instant::now();
        let mut last_bytes_at_report = downloaded;

        // Inkrementální SHA256 — hashujeme chunky rovnou při stahování, ať se
        // vyhneme pomalému druhému přečtení celého (mnohaGB) souboru po
        // dokončení (to způsobovalo „zaseknutí" na 99,9 %/100 %). Při resume
        // (navazujeme na existující .part) hash předchozí části neznáme, takže
        // sidecar v tom případě přeskočíme.
        let mut hasher: Option<Sha256> = (initial_downloaded == 0).then(Sha256::new);

        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                on_progress(DownloadProgress {
                    model_id: model.id.clone(),
                    downloaded,
                    total,
                    speed_bps: 0,
                    status: DownloadStatus::Cancelled,
                });
                return Err(anyhow!("Stahování zrušeno uživatelem."));
            }

            let chunk = chunk.with_context(|| "Chyba čtení streamu")?;
            file.write_all(&chunk).await?;
            if let Some(h) = hasher.as_mut() {
                h.update(&chunk);
            }
            downloaded += chunk.len() as u64;

            if last_report.elapsed() >= Duration::from_millis(200) {
                let elapsed = last_report.elapsed().as_secs_f64();
                let delta = downloaded.saturating_sub(last_bytes_at_report);
                let speed = if elapsed > 0.0 {
                    (delta as f64 / elapsed) as u64
                } else {
                    0
                };
                on_progress(DownloadProgress {
                    model_id: model.id.clone(),
                    downloaded,
                    total,
                    speed_bps: speed,
                    status: DownloadStatus::InProgress,
                });
                last_report = Instant::now();
                last_bytes_at_report = downloaded;
            }
        }

        file.sync_all().await?;
        drop(file);

        // Úplnost: pokud server udal Content-Length, ověř že jsme stáhli vše.
        // Neúplný soubor (server zavřel spojení) by se jinak tiše uložil jako
        // validní model a spadl až při načítání.
        if let Some(expected) = expected_final {
            if downloaded != expected {
                let _ = tokio::fs::remove_file(&temp).await;
                anyhow::bail!(
                    "Neúplné stažení: {} z {} bajtů (server přerušil spojení). Zkus stáhnout znovu.",
                    downloaded,
                    expected
                );
            }
        }

        // Atomický rename .part -> .gguf
        tokio::fs::rename(&temp, &target)
            .await
            .with_context(|| format!("Rename {} -> {} selhal", temp.display(), target.display()))?;

        let elapsed = start.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 {
            (downloaded as f64 / elapsed) as u64
        } else {
            0
        };

        // Dokončení hlásíme HNED po renamu — žádné pomalé druhé čtení souboru,
        // takže UI nezůstane viset na 100 %.
        on_progress(DownloadProgress {
            model_id: model.id.clone(),
            downloaded,
            total,
            speed_bps: avg_speed,
            status: DownloadStatus::Completed,
        });

        // Integrita: sidecar `<filename>.sha256` z inkrementálně počítaného
        // hashe (instantní `finalize`). Slouží k pozdější detekci poškození
        // i manuální verifikaci. Při resume hash nemáme → sidecar přeskočíme.
        if let Some(h) = hasher {
            let hash = format!("{:x}", h.finalize());
            let mut sidecar = target.clone().into_os_string();
            sidecar.push(".sha256"); // model.gguf → model.gguf.sha256
            let _ = tokio::fs::write(PathBuf::from(sidecar), &hash).await;
            tracing::info!(model = %model.id, sha256 = %hash, "SHA256 uložen");
        }

        tracing::info!(
            model = %model.id,
            path = %target.display(),
            size_mb = downloaded / 1_048_576,
            duration_s = elapsed,
            "Stažení dokončeno"
        );
        Ok(target)
    }

    // ---- Paralelní stahování ------------------------------------------------

    /// Stáhne soubor osmi spojeními naráz.
    ///
    /// `.part` se předalokuje na plnou velikost a vedle něj se drží mapa
    /// hotových úseků, takže přerušení zahodí nejvýš rozdělané kusy.
    async fn download_parallel<F>(
        &self,
        model: &DownloadTarget,
        total: u64,
        cancel: CancellationToken,
        on_progress: Arc<F>,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let paths = DownloadPaths::for_model(&self.models_dir, &model.filename);
        let (part, map_path) = (paths.part.clone(), paths.map.clone());

        let chunks = chunk_plan::plan(total, chunk_plan::DEFAULT_CHUNK_SIZE);
        let map = self
            .load_or_create_map(&part, &map_path, &chunks, total)
            .await?;

        let done_bytes = map.done_bytes(&chunks);
        let pending = map.pending(&chunks);

        on_progress(DownloadProgress {
            model_id: model.id.clone(),
            downloaded: done_bytes,
            total: Some(total),
            speed_bps: 0,
            status: DownloadStatus::Starting,
        });

        if pending.is_empty() {
            return self
                .finish_parallel(model, &paths, total, &map, &chunks, on_progress)
                .await;
        }

        tracing::info!(
            model = %model.id,
            todo = pending.len(),
            total_chunks = chunks.len(),
            "Stahuji úseky"
        );

        // Předalokace: bez nastavené délky by zápis na offset musel soubor
        // postupně roztahovat, což je na 15 GB znatelné a fragmentuje disk.
        {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&part)
                .await
                .with_context(|| format!("Nelze otevřít {}", part.display()))?;
            file.set_len(total).await?;
        }

        let queue = Arc::new(Mutex::new(pending));
        let map = Arc::new(Mutex::new(map));
        let downloaded = Arc::new(AtomicU64::new(done_bytes));
        let workers = chunk_plan::worker_count(
            queue.lock().expect("fronta").len(),
            chunk_plan::DEFAULT_WORKERS,
        );

        let reporter = self.spawn_reporter(
            model.id.clone(),
            total,
            done_bytes,
            Arc::clone(&downloaded),
            Arc::clone(&on_progress),
            cancel.clone(),
        );

        let mut tasks = tokio::task::JoinSet::new();
        for worker in 0..workers {
            let this = self.clone();
            let url = model.url.clone();
            let part = part.clone();
            let map_path = map_path.clone();
            let queue = Arc::clone(&queue);
            let map = Arc::clone(&map);
            let downloaded = Arc::clone(&downloaded);
            let cancel = cancel.clone();

            tasks.spawn(async move {
                loop {
                    let Some(chunk) = queue.lock().expect("fronta").pop() else {
                        return Ok::<_, anyhow::Error>(());
                    };
                    if cancel.is_cancelled() {
                        return Ok(());
                    }

                    this.fetch_chunk(&url, &part, chunk, &downloaded, &cancel)
                        .await
                        .with_context(|| format!("úsek {} (worker {worker})", chunk.index))?;

                    // Mapa se ukládá až po dokončení celého úseku — kdyby se
                    // zapsala dřív, navázání by kus přeskočilo jako hotový.
                    let bytes = {
                        let mut map = map.lock().expect("mapa");
                        map.mark_done(chunk.index);
                        map.to_bytes()
                    };
                    tokio::fs::write(&map_path, &bytes).await.ok();
                }
            });
        }

        let mut failure: Option<anyhow::Error> = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if failure.is_none() {
                        // Ostatní workery zastavíme, ať nedrží linku pro
                        // stahování, které stejně skončí chybou.
                        cancel.cancel();
                        failure = Some(e);
                    }
                }
                Err(e) => {
                    if failure.is_none() {
                        cancel.cancel();
                        failure = Some(anyhow!("worker spadl: {e}"));
                    }
                }
            }
        }
        reporter.abort();

        if cancel.is_cancelled() && failure.is_none() {
            on_progress(DownloadProgress {
                model_id: model.id.clone(),
                downloaded: downloaded.load(Ordering::Relaxed),
                total: Some(total),
                speed_bps: 0,
                status: DownloadStatus::Cancelled,
            });
            return Err(anyhow!("Stahování zrušeno uživatelem."));
        }
        if let Some(e) = failure {
            // `.part` i mapa zůstávají — příště se naváže.
            return Err(e);
        }

        let map = map.lock().expect("mapa").clone();
        self.finish_parallel(model, &paths, total, &map, &chunks, on_progress)
            .await
    }

    /// Načte mapu úseků, nebo začne načisto.
    ///
    /// Mapa se zahodí, kdykoli neodpovídá — jiná velikost souboru, jiné dělení,
    /// chybějící `.part`. Navázat na nesouhlasící `.part` by dalo soubor
    /// správné velikosti a poškozeného obsahu, což je horší než stáhnout znovu.
    async fn load_or_create_map(
        &self,
        part: &Path,
        map_path: &Path,
        chunks: &[FileChunk],
        total: u64,
    ) -> Result<ChunkMap> {
        let part_len = tokio::fs::metadata(part).await.map(|m| m.len()).ok();
        let stored = tokio::fs::read(map_path).await.ok();

        if let (Some(len), Some(bytes)) = (part_len, stored) {
            if len == total {
                if let Some(map) = ChunkMap::from_bytes(&bytes, chunks.len()) {
                    tracing::info!(
                        done = map.done_count(),
                        total = chunks.len(),
                        "Navazuji na rozdělané stahování"
                    );
                    return Ok(map);
                }
            }
            tracing::warn!("Rozdělané stahování neodpovídá souboru — začínám znovu");
        }

        tokio::fs::remove_file(part).await.ok();
        tokio::fs::remove_file(map_path).await.ok();
        Ok(ChunkMap::new(chunks.len()))
    }

    /// Stáhne jeden úsek a zapíše ho na jeho místo v `.part`.
    async fn fetch_chunk(
        &self,
        url: &str,
        part: &Path,
        chunk: FileChunk,
        downloaded: &AtomicU64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        use futures_util::StreamExt;

        let mut req = self
            .client
            .get(url)
            .header(reqwest::header::RANGE, chunk.range_header());
        if let Some(token) = &self.hf_token {
            req = req.bearer_auth(token);
        }

        let response = req.send().await.context("požadavek na úsek selhal")?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(anyhow!("server odmítl Range (HTTP {})", response.status()));
        }

        // Vlastní handle na worker: každý píše do jiné části souboru, takže
        // si navzájem nepřekáží.
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(part)
            .await
            .with_context(|| format!("Nelze otevřít {}", part.display()))?;
        file.seek(std::io::SeekFrom::Start(chunk.offset)).await?;

        let mut stream = response.bytes_stream();
        let mut written: u64 = 0;

        while let Some(next) = stream.next().await {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let bytes = next.context("chyba čtení streamu")?;
            file.write_all(&bytes).await?;
            written += bytes.len() as u64;
            downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        file.flush().await?;

        if written != chunk.length && !cancel.is_cancelled() {
            // Kratší úsek by se do mapy zapsal jako hotový a v souboru by
            // zůstala díra plná nul.
            return Err(anyhow!(
                "úsek {} má {written} B místo {} B",
                chunk.index,
                chunk.length
            ));
        }
        Ok(())
    }

    /// Hlásí průběh v pravidelném rytmu — workery samy nereportují, jinak by
    /// se hlášení osmi spojení mlátila mezi sebou.
    fn spawn_reporter<F>(
        &self,
        model_id: String,
        total: u64,
        start_bytes: u64,
        downloaded: Arc<AtomicU64>,
        on_progress: Arc<F>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        tokio::spawn(async move {
            let mut last = Instant::now();
            let mut last_bytes = start_bytes;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }

                let now = downloaded.load(Ordering::Relaxed);
                let window = last.elapsed().as_secs_f64();
                let speed = if window > 0.0 {
                    (now.saturating_sub(last_bytes) as f64 / window) as u64
                } else {
                    0
                };
                last = Instant::now();
                last_bytes = now;

                on_progress(DownloadProgress {
                    model_id: model_id.clone(),
                    downloaded: now,
                    total: Some(total),
                    speed_bps: speed,
                    status: DownloadStatus::InProgress,
                });
            }
        })
    }

    /// Ověří velikost, přejmenuje `.part` na cílový soubor a uklidí mapu.
    async fn finish_parallel<F>(
        &self,
        model: &DownloadTarget,
        paths: &DownloadPaths,
        total: u64,
        map: &ChunkMap,
        chunks: &[FileChunk],
        on_progress: Arc<F>,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let (part, map_path, target) = (&paths.part, &paths.map, &paths.target);
        // Velikost souboru tu nic nedokazuje: `.part` je předalokovaný na
        // plnou délku, takže sedí i s dírami plnými nul. Jediný důkaz
        // úplnosti je mapa úseků.
        if !map.all_done() {
            return Err(anyhow!(
                "Neúplné stažení: hotovo {} z {} úseků ({} z {total} B). Zkus to znovu.",
                map.done_count(),
                chunks.len(),
                map.done_bytes(chunks)
            ));
        }

        let actual = tokio::fs::metadata(part).await?.len();
        if actual != total {
            return Err(anyhow!(
                "Neúplné stažení: {actual} z {total} bajtů. Zkus to znovu."
            ));
        }

        tokio::fs::rename(part, target)
            .await
            .with_context(|| format!("Rename {} -> {} selhal", part.display(), target.display()))?;
        tokio::fs::remove_file(map_path).await.ok();

        on_progress(DownloadProgress {
            model_id: model.id.clone(),
            downloaded: total,
            total: Some(total),
            speed_bps: 0,
            status: DownloadStatus::Completed,
        });

        tracing::info!(
            model = %model.id,
            path = %target.display(),
            size_mb = total / 1_048_576,
            "Stažení dokončeno"
        );
        Ok(target.to_path_buf())
    }
}

/// Kam se během stahování zapisuje: rozdělaný soubor, mapa úseků a cíl.
struct DownloadPaths {
    part: PathBuf,
    map: PathBuf,
    target: PathBuf,
}

impl DownloadPaths {
    fn for_model(dir: &Path, filename: &str) -> Self {
        Self {
            part: dir.join(format!("{filename}.part")),
            map: dir.join(format!("{filename}.part.map")),
            target: dir.join(filename),
        }
    }
}

/// Z hlavičky `Content-Range: bytes 0-0/16796011072` vytáhne celkovou velikost.
fn parse_content_range_total(value: &str) -> Option<u64> {
    let total = value.rsplit('/').next()?.trim();
    // Server nemusí velikost znát — pak posílá `*`.
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_total_size_from_content_range() {
        assert_eq!(
            parse_content_range_total("bytes 0-0/16796011072"),
            Some(16_796_011_072)
        );
        assert_eq!(parse_content_range_total("bytes 0-99/100"), Some(100));
    }

    #[test]
    fn unknown_total_is_not_a_size() {
        // `*` znamená „velikost neznám" — brát to jako nulu by rozbilo plán.
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("nesmysl"), None);
    }
}
