use std::collections::{BTreeSet, BTreeMap};
use indexmap::IndexMap;

use crate::connect::Connection;

#[derive(Debug, Clone, edgedb_tokio::Queryable)]
// TODO(tailhook) this has to be open-ended enumeration
pub(crate) enum MigrationGeneratedBy {
    DevMode,
    DDLStatement,
}

pub(crate) trait SortableMigration {
    type ParentsIter<'a>: Iterator<Item = &'a String> where Self: 'a;
    fn name(&self) -> &str;
    fn is_root(&self) -> bool;
    fn iter_parents<'a>(&'a self) -> Self::ParentsIter<'a>;
}

// Database migration record
#[derive(Debug, Clone, edgedb_tokio::Queryable)]
pub(crate) struct DBMigration {
    pub(crate) name: String,
    pub(crate) script: String,
    pub(crate) parent_names: Vec<String>,
    pub(crate) generated_by: Option<MigrationGeneratedBy>,
}

impl SortableMigration for DBMigration {
    type ParentsIter<'a> = std::slice::Iter<'a, String>;

    fn name(&self) -> &str {
        &self.name
    }

    fn is_root(&self) -> bool {
        self.parent_names.is_empty()
    }

    fn iter_parents<'a>(&'a self) -> Self::ParentsIter<'a> {
        self.parent_names.iter()
    }
}

pub(crate) fn linearize_db_migrations<M>(
    migrations: Vec<M>,
) -> IndexMap<String, M> where M: SortableMigration + Clone {
    let mut by_parent = BTreeMap::new();
    for item in &migrations {
        for parent in item.iter_parents() {
            by_parent.entry(parent.clone())
                .or_insert_with(Vec::new)
                .push(item.clone());
        }
    }
    let mut output = IndexMap::new();
    let mut visited = BTreeSet::new();
    let mut queue = migrations.iter()
        .filter(|item| item.is_root())
        .map(|item| item.clone())
        .collect::<Vec<_>>();
    while let Some(item) = queue.pop() {
        output.insert(item.name().to_owned(), item.clone());
        visited.insert(item.name().to_string());
        if let Some(children) = by_parent.remove(item.name()) {
            for child in children {
                if !visited.contains(child.name()) {
                    queue.push(child.clone());
                }
            }
        }
    }
    return output
}

pub(crate) async fn read_all(
    cli: &mut Connection,
    fetch_script: bool,
    include_dev_mode: bool,
) -> anyhow::Result<IndexMap<String, DBMigration>> {
    let migrations = cli
        .query::<DBMigration, _>(
            r###"
            SELECT schema::Migration {
                name,
                script := .script if <bool>$0 else "",
                parent_names := .parents.name,
                generated_by,
            }
            FILTER
                <bool>$1
                OR .generated_by ?!= schema::MigrationGeneratedBy.DevMode
            "###,
            &(fetch_script, include_dev_mode),
        )
        .await?;
    Ok(linearize_db_migrations(migrations))
}

pub(crate) async fn find_by_prefix(
    cli: &mut Connection,
    prefix: &str,
) -> Result<Option<DBMigration>, anyhow::Error>
{
    let mut all_similar = cli.query::<DBMigration, _>(r###"
        SELECT schema::Migration {
            name,
            script,
            parent_names := .parents.name,
            generated_by,
        }
        FILTER .name LIKE <str>$0
        "###,
        &(format!("{}%", prefix),),
    ).await?;
    if all_similar.is_empty() {
        return Ok(None);
    }
    if all_similar.len() > 1 {
        anyhow::bail!("more than one migration matches prefix {:?}", prefix);
    }
    return Ok(all_similar.pop())
}
