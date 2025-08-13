use anyhow::Result;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use tokio::task::spawn_blocking;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

pub type SqlitePool = Pool<ConnectionManager<SqliteConnection>>;

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = crate::schema::channels)]
pub struct ChannelRow {
    pub id: i32,
    pub sgtid: i32,
    pub name: String,
    pub url: String,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = crate::schema::channels)]
pub struct NewChannel<'a> {
    pub sgtid: i32,
    pub name: &'a str,
    pub url: &'a str,
}

#[derive(Debug, Clone)]
pub struct NewChannelOwned {
    pub sgtid: i32,
    pub name: String,
    pub url: String,
}

impl<'a> From<NewChannel<'a>> for NewChannelOwned {
    fn from(value: NewChannel<'a>) -> Self {
        Self {
            sgtid: value.sgtid,
            name: value.name.to_string(),
            url: value.url.to_string(),
        }
    }
}

pub fn init_pool(database_url: &str) -> Result<SqlitePool> {
    let manager = ConnectionManager::<SqliteConnection>::new(database_url);
    let pool = Pool::builder().build(manager)?;
    {
        let mut conn = pool.get()?;
        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("migration error: {e}"))?;
    }
    Ok(pool)
}

fn upsert_channels_sync(conn: &mut SqliteConnection, channels: &[NewChannel]) -> Result<()> {
    use crate::schema::channels::dsl as ch;
    for c in channels {
        diesel::insert_into(ch::channels)
            .values(c)
            .on_conflict(ch::sgtid)
            .do_update()
            .set((ch::name.eq(c.name), ch::url.eq(c.url)))
            .execute(conn)?;
    }
    Ok(())
}

fn load_channels_sync(conn: &mut SqliteConnection) -> Result<Vec<ChannelRow>> {
    use crate::schema::channels::dsl as ch;
    let rows = ch::channels.order(ch::id.asc()).load::<ChannelRow>(conn)?;
    Ok(rows)
}

fn channel_at_sync(conn: &mut SqliteConnection, index: usize) -> Result<Option<ChannelRow>> {
    use crate::schema::channels::dsl as ch;
    let mut rows = ch::channels
        .order(ch::id.asc())
        .offset(index as i64)
        .limit(1)
        .load::<ChannelRow>(conn)?;
    Ok(rows.pop())
}

// Async wrappers to avoid blocking the async runtime. These offload Diesel calls to a blocking thread.
pub async fn load_channels(pool: SqlitePool) -> Result<Vec<ChannelRow>> {
    spawn_blocking(move || {
        let mut conn = pool.get()?;
        load_channels_sync(&mut conn)
    })
    .await
    .map_err(|e| anyhow::anyhow!("JoinError: {e}"))?
}

pub async fn channel_at(pool: SqlitePool, index: usize) -> Result<Option<ChannelRow>> {
    spawn_blocking(move || {
        let mut conn = pool.get()?;
        channel_at_sync(&mut conn, index)
    })
    .await
    .map_err(|e| anyhow::anyhow!("JoinError: {e}"))?
}

pub async fn upsert_channels(pool: SqlitePool, channels: Vec<NewChannelOwned>) -> Result<()> {
    spawn_blocking(move || {
        let mut conn = pool.get()?;
        // Borrow from owned for Insertable API
        let borrowed: Vec<NewChannel> = channels
            .iter()
            .map(|c| NewChannel {
                sgtid: c.sgtid,
                name: &c.name,
                url: &c.url,
            })
            .collect();
        upsert_channels_sync(&mut conn, &borrowed)
    })
    .await
    .map_err(|e| anyhow::anyhow!("JoinError: {e}"))?
}
