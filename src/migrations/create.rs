use async_std::path::{Path, PathBuf};
use async_std::fs;
use async_std::io;
use async_std::io::prelude::WriteExt;
use async_std::stream::StreamExt;
use fn_error_context::context;
use edgedb_derive::Queryable;
use edgedb_protocol::value::Value;
use edgeql_parser::preparser::{full_statement, is_empty};
use serde::Deserialize;

use crate::commands::Options;
use crate::commands::parser::CreateMigration;
use crate::client::Connection;
use crate::migrations::context::Context;
use crate::migrations::migration;
use crate::migrations::sourcemap::{Builder, SourceMap};

const SAFE_CONFIDENCE: f64 = 0.99999;

pub enum SourceName {
    Prefix,
    Suffix,
    File(PathBuf),
}

#[derive(Deserialize, Debug)]
pub struct RequiredUserInput {
    name: String,
    prompt: String,
}

#[derive(Deserialize, Debug)]
pub struct StatementProposal {
    pub text: String,
    #[serde(default)]
    pub required_user_input: Vec<RequiredUserInput>,
}

#[derive(Deserialize, Debug)]
pub struct Proposal {
    pub statements: Vec<StatementProposal>,
    pub confidence: f64,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Deserialize, Queryable, Debug)]
#[edgedb(json)]
pub struct CurrentMigration {
    pub confirmed: Vec<String>,
    pub proposed: Vec<Proposal>,
}

#[context("could not read schema file {}", path.display())]
async fn read_schema_file(path: &Path) -> anyhow::Result<String> {
    let data = fs::read_to_string(path).await?;
    let mut offset = 0;
    loop {
        match full_statement(data[offset..].as_bytes(), None) {
            Ok(shift) => offset += shift,
            Err(_) => {
                if !is_empty(&data[offset..]) {
                    anyhow::bail!("final statement does not end with a semicolon");
                }
                return Ok(data);
            }
        }
    }
}

#[context("could not read schema in {}", ctx.schema_dir.display())]
pub async fn gen_start_migration(ctx: &Context)
    -> anyhow::Result<(String, SourceMap<SourceName>)>
{
    let mut bld = Builder::new();
    bld.add_lines(SourceName::Prefix, "START MIGRATION TO {");
    let mut dir = fs::read_dir(&ctx.schema_dir).await?;
    while let Some(item) = dir.next().await.transpose()? {
        let fname = item.file_name();
        let lossy_name = fname.to_string_lossy();
        if lossy_name.starts_with(".") || !lossy_name.ends_with(".esdl")
            || !item.file_type().await?.is_file()
        {
            continue;
        }
        let path = item.path();
        let chunk = read_schema_file(&path).await?;
        bld.add_lines(SourceName::File(path), &chunk);
    }
    bld.add_lines(SourceName::Suffix, "};");
    Ok(bld.done())
}

async fn run_non_interactive(ctx: &Context, cli: &mut Connection, index: u64)
    -> anyhow::Result<()>
{
    let descr = loop {
        let data = cli.query_row::<CurrentMigration>(
            "DESCRIBE CURRENT MIGRATION AS JSON",
            &Value::empty_tuple(),
        ).await?;
        if data.proposed.is_empty() {
            break data;
        }
        for proposal in data.proposed {
            if proposal.confidence >= SAFE_CONFIDENCE {
                for statement in proposal.statements {
                    if !statement.required_user_input.is_empty() {
                        for input in statement.required_user_input {
                            eprintln!("Input required: {}", input.prompt);
                        }
                        anyhow::bail!(
                            "cannot apply `{}` without user input",
                            statement.text);
                    }
                    cli.execute(&statement.text).await?;
                }
            }
        }
    };
    // TODO(tailhook) read this from describe transaction
    let parent: Option<String> = cli.query_row_opt(r###"
            WITH Last := (SELECT schema::Migration
                          FILTER NOT EXISTS .<parents[IS schema::Migration])
            SELECT name := Last.name
        "###, &Value::empty_tuple()).await?;
    let parent = parent.as_ref().map(|x| &x[..]).unwrap_or("initial");
    write_migration(ctx, &descr, parent, index).await?;
    Ok(())
}

pub async fn write_migration(ctx: &Context,
    descr: &CurrentMigration, parent: &str, index: u64)
    -> anyhow::Result<()>
{
    let dir = ctx.schema_dir.join("migrations");
    let filename = dir.join(format!("{:05}.edgeql", index));
    _write_migration(descr, parent, filename.as_ref()).await
}

#[context("could not write migration file {}", filepath.display())]
async fn _write_migration(descr: &CurrentMigration, parent: &str,
    filepath: &Path)
    -> anyhow::Result<()>
{
    let statements = descr.confirmed.iter()
        .map(|s| s.clone() + ";")
        .collect::<Vec<_>>();
    let mut hasher = migration::Hasher::new(&parent);
    for statement in &statements {
        hasher.source(&statement)?;
    }
    let id = hasher.make_id();
    let dir = filepath.parent().unwrap();
    let tmp_file = dir.join(format!(".~{}.tmp",
        filepath.file_name().unwrap().to_str().unwrap()));
    if !filepath.exists().await {
        fs::create_dir_all(&dir).await?;
    }
    fs::remove_file(&tmp_file).await.ok();
    let mut file = io::BufWriter::new(fs::File::create(&tmp_file).await?);
    file.write(format!("CREATE MIGRATION {}\n", id).as_bytes()).await?;
    file.write(format!("    ONTO {}\n", parent).as_bytes()).await?;
    file.write(b"{\n").await?;
    for statement in &statements {
        for line in statement.lines() {
            file.write(&format!("  {}\n", line).as_bytes()).await?;
        }
    }
    file.write(b"};\n").await?;
    file.flush().await?;
    drop(file);
    fs::rename(&tmp_file, &filepath).await?;
    Ok(())
}

pub async fn create(cli: &mut Connection, _options: &Options,
    create: &CreateMigration)
    -> Result<(), anyhow::Error>
{
    let ctx = Context::from_config(&create.cfg);
    let migrations = migration::read_all(&ctx, true).await?;
    let (text, sourcemap) = gen_start_migration(&ctx).await?;
    cli.execute(text).await?;
    let db_migration: Option<String> = cli.query_row_opt(r###"
            WITH Last := (SELECT schema::Migration
                          FILTER NOT EXISTS .<parents[IS schema::Migration])
            SELECT name := Last.name
        "###, &Value::empty_tuple()).await?;
    if db_migration.as_ref() != migrations.keys().last() {
        anyhow::bail!("Database must be updated to the last miration \
            on the filesystem for `create-migration`. Run:\n  \
            edgedb migrate");
    }

    let exec = if create.non_interactive {
        run_non_interactive(&ctx, cli, migrations.len() as u64 +1).await
    } else {
        // TODO(tailhook)
        anyhow::bail!("interactive mode is not implemented yet, try:\n  \
            edgedb create-migration --non-interactive");
    };
    let abort = cli.execute("ABORT MIGRATION").await;
    exec.and(abort)?;
    Ok(())
}