//! Historie konverzací v SQLite.
//!
//! Ukládá se po každé zprávě, ne až při zavření okna — pád aplikace uprostřed
//! dlouhé odpovědi nesmí znamenat ztrátu celé konverzace. Zápis proto běží
//! v transakci: buď se uloží hlavička i všechny zprávy, nebo nic.
//!
//! Zprávy se při načítání seznamu **nečtou**. Historie o stovkách konverzací
//! má desítky megabajtů textu a tahat je do paměti kvůli tomu, aby se vlevo
//! vypsaly názvy, nedává smysl.

use std::{path::Path, str::FromStr};

use anvil_domain::{
    conversation::{BranchPoint, Conversation, Message, Role},
    error::{DomainError, DomainResult},
    history::{self, ConversationSummary},
    id::{ConversationId, MessageId},
    model::ModelId,
    ports::ConversationStore,
};
use async_trait::async_trait;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use time::OffsetDateTime;

pub struct SqliteConversationStore {
    pool: SqlitePool,
}

impl SqliteConversationStore {
    /// Otevře (a případně založí) databázi na dané cestě.
    pub async fn open(path: &Path) -> DomainResult<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                DomainError::storage(format!("nelze vytvořit {}: {e}", parent.display()))
            })?;
        }

        // `mode=rwc` databázi založí, když ještě není.
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|e| DomainError::storage(format!("nelze otevřít {}: {e}", path.display())))?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Databáze v paměti — pro testy.
    pub async fn in_memory() -> DomainResult<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|e| DomainError::storage(format!("nelze otevřít paměťovou databázi: {e}")))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> DomainResult<()> {
        // WAL: čtení seznamu neblokuje probíhající zápis odpovědi.
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        // Bez tohohle SQLite cizí klíče ignoruje a mazání konverzace by
        // po sobě nechalo osiřelé zprávy.
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&self.pool)
            .await
            .map_err(storage)?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS conversations (
                id                 TEXT PRIMARY KEY,
                title              TEXT NOT NULL,
                model_id           TEXT,
                summary            TEXT,
                compacted_through  TEXT,
                pinned             INTEGER NOT NULL DEFAULT 0,
                sort_order         INTEGER NOT NULL DEFAULT 0,
                parent_id          TEXT,
                branch_at_message  TEXT,
                created_at         TEXT NOT NULL,
                updated_at         TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;

        // Databáze z dřívějších verzí už existuje a `CREATE TABLE IF NOT
        // EXISTS` na ni nesáhne. Bez dopsání sloupců by aplikace po
        // aktualizaci přestala číst vlastní historii.
        //
        // Odkaz na rodiče schválně **není** cizí klíč: smazání rodiče nemá
        // vzít s sebou větve ani zablokovat mazání. Zůstane po něm ID, které
        // se v seznamu prostě nenajde, a větev žije dál sama za sebe.
        self.add_column_if_missing("conversations", "parent_id", "parent_id TEXT")
            .await?;
        self.add_column_if_missing(
            "conversations",
            "branch_at_message",
            "branch_at_message TEXT",
        )
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id               TEXT PRIMARY KEY,
                conversation_id  TEXT NOT NULL
                    REFERENCES conversations(id) ON DELETE CASCADE,
                position         INTEGER NOT NULL,
                role             TEXT NOT NULL,
                content          TEXT NOT NULL,
                token_count      INTEGER,
                created_at       TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;

        // Bez indexu by se při každém otevření konverzace procházely všechny
        // zprávy v databázi.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_conversation
             ON messages(conversation_id, position)",
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;

        Ok(())
    }

    /// Přidá sloupec, pokud v tabulce ještě není.
    ///
    /// SQLite neumí `ADD COLUMN IF NOT EXISTS`, takže se stav zjišťuje
    /// dotazem. Názvy tabulky a sloupce se do SQL vkládají textem — vázat
    /// je jako parametry nejde a všechna volání jsou konstanty přímo v
    /// tomhle souboru, takže se sem nic zvenčí nedostane.
    async fn add_column_if_missing(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> DomainResult<()> {
        let sloupce = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
        let uz_je = sloupce
            .iter()
            .filter_map(|r| r.try_get::<String, _>("name").ok())
            .any(|n| n == column);
        if uz_je {
            return Ok(());
        }

        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {definition}"))
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        tracing::info!(table, column, "Databaze doplnena o sloupec");
        Ok(())
    }
}

fn storage(e: sqlx::Error) -> DomainError {
    DomainError::storage(e.to_string())
}

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn role_from_str(raw: &str) -> Role {
    match raw {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        // Neznámá role z novější verze appky nesmí shodit načtení celé
        // konverzace — bere se jako systémová a do promptu se stejně nedostane.
        _ => Role::System,
    }
}

fn parse_time(raw: &str) -> OffsetDateTime {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
}

fn format_time(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[async_trait]
impl ConversationStore for SqliteConversationStore {
    async fn list(&self) -> DomainResult<Vec<ConversationSummary>> {
        let rows = sqlx::query(
            r#"
            SELECT c.id, c.title, c.pinned, c.sort_order, c.updated_at, c.model_id, c.parent_id,
                   (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) AS message_count
            FROM conversations c
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        let mut out: Vec<ConversationSummary> = rows
            .into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                Some(ConversationSummary {
                    id: ConversationId::from_str(&id).ok()?,
                    title: r.try_get("title").unwrap_or_default(),
                    pinned: r.try_get::<i64, _>("pinned").unwrap_or(0) != 0,
                    sort_order: r.try_get("sort_order").unwrap_or(0),
                    updated_at: parse_time(
                        &r.try_get::<String, _>("updated_at").unwrap_or_default(),
                    ),
                    message_count: r.try_get::<i64, _>("message_count").unwrap_or(0) as u32,
                    model_id: r
                        .try_get::<Option<String>, _>("model_id")
                        .ok()
                        .flatten()
                        .and_then(|m| ModelId::parse(m).ok()),
                    parent_id: r
                        .try_get::<Option<String>, _>("parent_id")
                        .ok()
                        .flatten()
                        .and_then(|p| ConversationId::from_str(&p).ok()),
                })
            })
            .collect();

        // Řazení dělá doména, ne SQL — je to pravidlo aplikace (připnuté
        // nahoře, stabilní pořadí) a patří tam, kde je k němu test.
        history::sort_for_display(&mut out);
        Ok(out)
    }

    async fn load(&self, id: ConversationId) -> DomainResult<Conversation> {
        let key = id.to_string();

        let row = sqlx::query("SELECT * FROM conversations WHERE id = ?")
            .bind(&key)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?
            .ok_or_else(|| DomainError::not_found(format!("konverzace {id}")))?;

        let messages = sqlx::query(
            "SELECT id, role, content, token_count, created_at
             FROM messages WHERE conversation_id = ? ORDER BY position",
        )
        .bind(&key)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .filter_map(|r| {
            let mid: String = r.try_get("id").ok()?;
            Some(Message {
                id: MessageId::from_str(&mid).ok()?,
                role: role_from_str(&r.try_get::<String, _>("role").unwrap_or_default()),
                content: r.try_get("content").unwrap_or_default(),
                token_count: r
                    .try_get::<Option<i64>, _>("token_count")
                    .ok()
                    .flatten()
                    .map(|t| t as u32),
                created_at: parse_time(&r.try_get::<String, _>("created_at").unwrap_or_default()),
            })
        })
        .collect();

        Ok(Conversation {
            id,
            title: row.try_get("title").unwrap_or_default(),
            model_id: row
                .try_get::<Option<String>, _>("model_id")
                .ok()
                .flatten()
                .and_then(|m| ModelId::parse(m).ok()),
            summary: row.try_get::<Option<String>, _>("summary").ok().flatten(),
            compacted_through: row
                .try_get::<Option<String>, _>("compacted_through")
                .ok()
                .flatten()
                .and_then(|m| MessageId::from_str(&m).ok()),
            pinned: row.try_get::<i64, _>("pinned").unwrap_or(0) != 0,
            sort_order: row.try_get("sort_order").unwrap_or(0),
            // Větev bez obou údajů není větev. Kdyby se jeden z nich ztratil,
            // je poctivější tvářit se jako samostatné vlákno než ukazovat
            // odkaz, který nikam nevede.
            branched_from: match (
                row.try_get::<Option<String>, _>("parent_id").ok().flatten(),
                row.try_get::<Option<String>, _>("branch_at_message")
                    .ok()
                    .flatten(),
            ) {
                (Some(parent), Some(at)) => ConversationId::from_str(&parent)
                    .ok()
                    .zip(MessageId::from_str(&at).ok())
                    .map(|(parent, at_message)| BranchPoint { parent, at_message }),
                _ => None,
            },
            messages,
            created_at: parse_time(&row.try_get::<String, _>("created_at").unwrap_or_default()),
            updated_at: parse_time(&row.try_get::<String, _>("updated_at").unwrap_or_default()),
        })
    }

    async fn save(&self, conversation: &Conversation) -> DomainResult<()> {
        let key = conversation.id.to_string();
        let mut tx = self.pool.begin().await.map_err(storage)?;

        sqlx::query(
            r#"
            INSERT INTO conversations
                (id, title, model_id, summary, compacted_through, pinned, sort_order,
                 parent_id, branch_at_message, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                model_id = excluded.model_id,
                summary = excluded.summary,
                compacted_through = excluded.compacted_through,
                pinned = excluded.pinned,
                sort_order = excluded.sort_order,
                updated_at = excluded.updated_at
            -- `parent_id` ani `branch_at_message` se schválně nepřepisují:
            -- odkud vlákno vzniklo, je fakt z okamžiku založení a žádné
            -- pozdější uložení ho nemá měnit.
            "#,
        )
        .bind(&key)
        .bind(&conversation.title)
        .bind(conversation.model_id.as_ref().map(ModelId::to_string))
        .bind(conversation.summary.as_deref())
        .bind(conversation.compacted_through.map(|m| m.to_string()))
        .bind(i64::from(conversation.pinned))
        .bind(conversation.sort_order)
        .bind(
            conversation
                .branched_from
                .as_ref()
                .map(|b| b.parent.to_string()),
        )
        .bind(
            conversation
                .branched_from
                .as_ref()
                .map(|b| b.at_message.to_string()),
        )
        .bind(format_time(conversation.created_at))
        .bind(format_time(conversation.updated_at))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        // Zprávy se přepisují celé. Inkrementální zápis by musel řešit
        // úpravy a mazání uprostřed; při desítkách zpráv se to nevyplatí.
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;

        for (position, message) in conversation.messages.iter().enumerate() {
            sqlx::query(
                "INSERT INTO messages
                   (id, conversation_id, position, role, content, token_count, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(message.id.to_string())
            .bind(&key)
            .bind(position as i64)
            .bind(role_to_str(message.role))
            .bind(&message.content)
            .bind(message.token_count.map(i64::from))
            .bind(format_time(message.created_at))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }

        tx.commit().await.map_err(storage)
    }

    async fn rename(&self, id: ConversationId, title: &str) -> DomainResult<()> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DomainError::validation("název nesmí být prázdný"));
        }
        sqlx::query("UPDATE conversations SET title = ? WHERE id = ?")
            .bind(title)
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }

    async fn set_pinned(&self, id: ConversationId, pinned: bool) -> DomainResult<()> {
        sqlx::query("UPDATE conversations SET pinned = ? WHERE id = ?")
            .bind(i64::from(pinned))
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }

    async fn reorder(&self, ids: &[ConversationId]) -> DomainResult<()> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        for (id, order) in history::apply_order(ids) {
            sqlx::query("UPDATE conversations SET sort_order = ? WHERE id = ?")
                .bind(order)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)
    }

    async fn delete(&self, id: ConversationId) -> DomainResult<()> {
        // Cizí klíč s ON DELETE CASCADE se postará o zprávy.
        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SqliteConversationStore {
        SqliteConversationStore::in_memory().await.unwrap()
    }

    fn konverzace(nazev: &str) -> Conversation {
        let mut c = Conversation::new(nazev);
        c.push(Message::user("dotaz").with_token_count(5));
        c.push(Message::assistant("odpověď").with_token_count(9));
        c
    }

    #[tokio::test]
    async fn ulozena_konverzace_se_nacte_zpatky() {
        let s = store().await;
        let c = konverzace("Test");
        s.save(&c).await.unwrap();

        let zpet = s.load(c.id).await.unwrap();
        assert_eq!(zpet.title, "Test");
        assert_eq!(zpet.messages.len(), 2);
        assert_eq!(zpet.messages[0].content, "dotaz");
        assert_eq!(zpet.messages[0].token_count, Some(5));
        assert_eq!(zpet.messages[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn vetev_i_rodic_zijou_vedle_sebe() {
        let s = store().await;
        let rodic = konverzace("Rodič");
        s.save(&rodic).await.unwrap();

        let mut vetev = rodic.branch_through(rodic.messages[0].id).unwrap();
        vetev.title = "Větev".into();
        s.save(&vetev).await.unwrap();

        // Uložení větve nesmí sáhnout na zprávy rodiče — ID zpráv jsou
        // v jedné tabulce primární klíč a sdílená by se přepsala.
        let rodic_zpet = s.load(rodic.id).await.unwrap();
        assert_eq!(rodic_zpet.messages.len(), 2);

        let vetev_zpet = s.load(vetev.id).await.unwrap();
        assert_eq!(vetev_zpet.messages.len(), 1);
        assert_eq!(vetev_zpet.messages[0].content, "dotaz");

        let odkud = vetev_zpet.branched_from.expect("větev zná rodiče");
        assert_eq!(odkud.parent, rodic.id);
        assert_eq!(odkud.at_message, rodic.messages[0].id);
    }

    #[tokio::test]
    async fn seznam_ukazuje_rodice_vetve() {
        let s = store().await;
        let rodic = konverzace("Rodič");
        s.save(&rodic).await.unwrap();
        let vetev = rodic.branch_through(rodic.messages[1].id).unwrap();
        s.save(&vetev).await.unwrap();

        let seznam = s.list().await.unwrap();
        let v = seznam.iter().find(|c| c.id == vetev.id).unwrap();
        assert_eq!(v.parent_id, Some(rodic.id));
        let r = seznam.iter().find(|c| c.id == rodic.id).unwrap();
        assert_eq!(r.parent_id, None);
    }

    #[tokio::test]
    async fn smazani_rodice_vetev_nezabije() {
        // Odkaz na rodiče není cizí klíč právě proto, aby tohle prošlo:
        // větev je samostatné vlákno s vlastní kopií historie.
        let s = store().await;
        let rodic = konverzace("Rodič");
        s.save(&rodic).await.unwrap();
        let vetev = rodic.branch_through(rodic.messages[1].id).unwrap();
        s.save(&vetev).await.unwrap();

        s.delete(rodic.id).await.unwrap();

        let zpet = s.load(vetev.id).await.unwrap();
        assert_eq!(zpet.messages.len(), 2);
        assert_eq!(
            zpet.branched_from.map(|b| b.parent),
            Some(rodic.id),
            "odkaz zůstane, i když cíl už neexistuje"
        );
    }

    /// Databáze uživatele vznikla dřív, než větvení existovalo. Tenhle test
    /// hlídá, že se po aktualizaci otevře a nepřijde o obsah — bez migrace
    /// by `SELECT c.parent_id` skončil chybou a aplikace by ukázala prázdný
    /// seznam místo historie.
    #[tokio::test]
    async fn stara_databaze_se_doplni_a_otevre() {
        let dir = tempfile::tempdir().unwrap();
        let cesta = dir.path().join("stara.db");

        {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{}?mode=rwc", cesta.display()))
                .await
                .unwrap();
            sqlx::query(
                r#"CREATE TABLE conversations (
                    id TEXT PRIMARY KEY, title TEXT NOT NULL, model_id TEXT,
                    summary TEXT, compacted_through TEXT,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL)"#,
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                r#"CREATE TABLE messages (
                    id TEXT PRIMARY KEY,
                    conversation_id TEXT NOT NULL
                        REFERENCES conversations(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL, role TEXT NOT NULL,
                    content TEXT NOT NULL, token_count INTEGER,
                    created_at TEXT NOT NULL)"#,
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO conversations (id, title, pinned, sort_order, created_at, updated_at)
                 VALUES ('11111111-1111-4111-8111-111111111111', 'Stará', 0, 0,
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO messages (id, conversation_id, position, role, content, created_at)
                 VALUES ('22222222-2222-4222-8222-222222222222',
                         '11111111-1111-4111-8111-111111111111', 0, 'user', 'starý dotaz',
                         '2026-01-01T00:00:00Z')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let s = SqliteConversationStore::open(&cesta).await.unwrap();

        let seznam = s.list().await.unwrap();
        assert_eq!(seznam.len(), 1);
        assert_eq!(seznam[0].title, "Stará");
        assert_eq!(seznam[0].parent_id, None);

        let nactena = s.load(seznam[0].id).await.unwrap();
        assert_eq!(nactena.messages[0].content, "starý dotaz");
        assert!(nactena.branched_from.is_none());

        // A po doplnění musí jít i zapsat větev.
        let vetev = nactena.branch_through(nactena.messages[0].id).unwrap();
        s.save(&vetev).await.unwrap();
        assert_eq!(
            s.load(vetev.id)
                .await
                .unwrap()
                .branched_from
                .unwrap()
                .parent,
            nactena.id
        );
    }

    #[tokio::test]
    async fn poradi_zprav_zustane() {
        let s = store().await;
        let mut c = Conversation::new("Pořadí");
        for i in 0..10 {
            c.push(Message::user(format!("zpráva {i}")));
        }
        s.save(&c).await.unwrap();

        let zpet = s.load(c.id).await.unwrap();
        assert_eq!(
            zpet.messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>(),
            (0..10).map(|i| format!("zpráva {i}")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn opakovane_ulozeni_nezdvoji_zpravy() {
        let s = store().await;
        let mut c = konverzace("Test");
        s.save(&c).await.unwrap();

        c.push(Message::user("další"));
        s.save(&c).await.unwrap();

        assert_eq!(s.load(c.id).await.unwrap().messages.len(), 3);
    }

    #[tokio::test]
    async fn souhrn_a_hranice_slouceni_prezijou() {
        let s = store().await;
        let mut c = konverzace("Se souhrnem");
        let hranice = c.messages[0].id;
        c.compact("shrnutí", hranice).unwrap();
        s.save(&c).await.unwrap();

        let zpet = s.load(c.id).await.unwrap();
        assert_eq!(zpet.summary.as_deref(), Some("shrnutí"));
        assert_eq!(zpet.compacted_through, Some(hranice));
        assert_eq!(zpet.visible_messages().len(), 1);
    }

    #[tokio::test]
    async fn seznam_neobsahuje_zpravy_ale_zna_jejich_pocet() {
        let s = store().await;
        s.save(&konverzace("A")).await.unwrap();

        let seznam = s.list().await.unwrap();
        assert_eq!(seznam.len(), 1);
        assert_eq!(seznam[0].message_count, 2);
    }

    #[tokio::test]
    async fn pripnute_jsou_v_seznamu_nahore() {
        let s = store().await;
        let mut prvni = konverzace("První");
        prvni.sort_order = 0;
        let mut druha = konverzace("Druhá");
        druha.sort_order = 1;
        s.save(&prvni).await.unwrap();
        s.save(&druha).await.unwrap();

        s.set_pinned(druha.id, true).await.unwrap();

        let seznam = s.list().await.unwrap();
        assert_eq!(seznam[0].title, "Druhá");
        assert!(seznam[0].pinned);
    }

    #[tokio::test]
    async fn prerovnani_se_projevi_v_seznamu() {
        let s = store().await;
        let a = konverzace("A");
        let b = konverzace("B");
        let c = konverzace("C");
        for k in [&a, &b, &c] {
            s.save(k).await.unwrap();
        }

        s.reorder(&[c.id, a.id, b.id]).await.unwrap();

        let nazvy: Vec<_> = s
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|x| x.title)
            .collect();
        assert_eq!(nazvy, vec!["C", "A", "B"]);
    }

    #[tokio::test]
    async fn prejmenovani_se_ulozi() {
        let s = store().await;
        let c = konverzace("Starý název");
        s.save(&c).await.unwrap();

        s.rename(c.id, "  Nový název  ").await.unwrap();
        assert_eq!(s.load(c.id).await.unwrap().title, "Nový název");
    }

    #[tokio::test]
    async fn prazdny_nazev_neprojde() {
        let s = store().await;
        let c = konverzace("Název");
        s.save(&c).await.unwrap();

        assert!(s.rename(c.id, "   ").await.is_err());
        assert_eq!(s.load(c.id).await.unwrap().title, "Název");
    }

    #[tokio::test]
    async fn smazani_odstrani_i_zpravy() {
        let s = store().await;
        let c = konverzace("Ke smazání");
        s.save(&c).await.unwrap();

        s.delete(c.id).await.unwrap();

        assert!(s.list().await.unwrap().is_empty());
        assert!(s.load(c.id).await.is_err());
        // Osiřelé zprávy by se jinak hromadily a nikdo by je nenašel.
        let zbyle: i64 = sqlx::query("SELECT COUNT(*) AS n FROM messages")
            .fetch_one(&s.pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(zbyle, 0);
    }

    #[tokio::test]
    async fn smazani_neexistujici_neni_chyba() {
        let s = store().await;
        assert!(s.delete(ConversationId::new()).await.is_ok());
    }

    #[tokio::test]
    async fn nacteni_neexistujici_je_chyba() {
        let s = store().await;
        assert!(s.load(ConversationId::new()).await.is_err());
    }

    #[tokio::test]
    async fn neznama_role_neshodi_nacteni() {
        // Konverzace uložená novější verzí appky se musí načíst i po downgradu.
        let s = store().await;
        let c = konverzace("Test");
        s.save(&c).await.unwrap();

        sqlx::query("UPDATE messages SET role = 'neco_noveho' WHERE conversation_id = ?")
            .bind(c.id.to_string())
            .execute(&s.pool)
            .await
            .unwrap();

        assert_eq!(s.load(c.id).await.unwrap().messages.len(), 2);
    }

    #[tokio::test]
    async fn databaze_na_disku_prezije_zavreni() {
        let dir = tempfile::tempdir().unwrap();
        let cesta = dir.path().join("hloubka").join("history.db");

        let c = {
            let s = SqliteConversationStore::open(&cesta).await.unwrap();
            let c = konverzace("Přežije");
            s.save(&c).await.unwrap();
            c
        };

        let s = SqliteConversationStore::open(&cesta).await.unwrap();
        assert_eq!(s.load(c.id).await.unwrap().title, "Přežije");
    }
}
