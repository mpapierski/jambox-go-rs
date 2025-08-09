use anyhow::Result;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

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

pub fn upsert_channels(conn: &mut SqliteConnection, channels: &[NewChannel]) -> Result<()> {
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

pub fn load_channels(conn: &mut SqliteConnection) -> Result<Vec<ChannelRow>> {
    use crate::schema::channels::dsl as ch;
    let rows = ch::channels
        .order(ch::sgtid.asc())
        .load::<ChannelRow>(conn)?;
    Ok(rows)
}

pub fn channel_at(conn: &mut SqliteConnection, index: usize) -> Result<Option<ChannelRow>> {
    use crate::schema::channels::dsl as ch;
    let mut rows = ch::channels
        .order(ch::sgtid.asc())
        .offset(index as i64)
        .limit(1)
        .load::<ChannelRow>(conn)?;
    Ok(rows.pop())
}
