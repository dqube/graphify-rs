//! Live PostgreSQL schema introspection for `--postgres`.
//!
//! Reads the catalog, reconstructs synthetic DDL, and feeds that text to the
//! ordinary SQL extractor, so a live database and a checked-in `schema.sql`
//! produce the same node shapes.
//!
//! Foreign keys are the exception: the SQL extractor only recognises `CREATE`
//! statements, so `ALTER TABLE … FOREIGN KEY` would be silently ignored. The
//! `references` edges are therefore emitted directly here, using the same node
//! id recipe the extractor uses, so they attach to the very nodes it created.
//!
//! Everything runs inside a `SERIALIZABLE READ ONLY DEFERRABLE` transaction —
//! introspection must never be able to write.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge};
use serde_json::Value;
use tokio_postgres::Config;

/// One row of `information_schema.tables`.
struct TableRow {
    schema: String,
    name: String,
    kind: String,
}

/// One row of `information_schema.views`.
struct ViewRow {
    schema: String,
    name: String,
    body: Option<String>,
}

/// One row of `information_schema.routines`.
struct RoutineRow {
    schema: String,
    name: String,
    kind: String,
    body: Option<String>,
    language: Option<String>,
}

/// One foreign-key constraint, read from `pg_catalog.pg_constraint`.
struct ForeignKeyRow {
    constraint: String,
    schema: String,
    table: String,
    columns: Vec<String>,
    foreign_schema: String,
    foreign_table: String,
    foreign_columns: Vec<String>,
}

/// Everything read from the catalog in one pass.
#[derive(Default)]
struct SchemaSnapshot {
    tables: Vec<TableRow>,
    views: Vec<ViewRow>,
    routines: Vec<RoutineRow>,
    foreign_keys: Vec<ForeignKeyRow>,
}

/// Double-quote an identifier only when it needs it.
///
/// Plain identifiers are left bare so labels read as `public.users` rather than
/// `public"."users` — the SQL extractor only strips quotes at the very ends of
/// the name it captures.
fn quote_ident(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name == name.to_ascii_lowercase();
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `schema.name`, the form that becomes the node label.
fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Collapse whitespace so each synthesized statement occupies exactly one line.
///
/// The SQL extractor anchors its patterns to the start of a line, so a view or
/// function body containing `create table …` at column 0 would otherwise be
/// picked up as a real object.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reconstruct DDL text from a catalog snapshot.
fn build_ddl(snap: &SchemaSnapshot) -> String {
    let mut out: Vec<String> = Vec::new();

    // Columns are not introspected — the graph models objects and their
    // relationships, so a placeholder column keeps the statement parseable.
    for t in &snap.tables {
        if t.kind == "BASE TABLE" {
            out.push(format!(
                "CREATE TABLE {} (id INT);",
                qualified(&t.schema, &t.name)
            ));
        }
    }

    // A NULL body means the role cannot read the definition; a stub still
    // records that the view exists.
    for v in &snap.views {
        let body = v
            .body
            .as_deref()
            .map(one_line)
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "SELECT 1".to_string());
        let body = body.trim_end_matches(';').to_string();
        out.push(format!(
            "CREATE VIEW {} AS {body};",
            qualified(&v.schema, &v.name)
        ));
    }

    // Procedures are written as FUNCTION so one pattern covers both. The
    // `$gfx$` dollar-quote tag avoids colliding with a `$$` inside the body.
    for r in &snap.routines {
        if r.kind != "FUNCTION" && r.kind != "PROCEDURE" {
            continue;
        }
        let lang = r
            .language
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("plpgsql")
            .to_ascii_lowercase();
        let body = r
            .body
            .as_deref()
            .map(one_line)
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "BEGIN SELECT 1; END;".to_string());
        out.push(format!(
            "CREATE FUNCTION {}() RETURNS void AS $gfx$ {body} $gfx$ LANGUAGE {lang};",
            qualified(&r.schema, &r.name)
        ));
    }

    // Recorded for completeness; the `references` edges are emitted separately
    // because the SQL extractor does not read ALTER statements.
    for f in &snap.foreign_keys {
        let cols = f
            .columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let ref_cols = f
            .foreign_columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({cols}) REFERENCES {}({ref_cols});",
            qualified(&f.schema, &f.table),
            quote_ident(&f.constraint),
            qualified(&f.foreign_schema, &f.foreign_table),
        ));
    }

    out.join("\n")
}

/// Turn a snapshot into nodes and edges anchored at `virtual_path`.
fn to_extraction(snap: &SchemaSnapshot, virtual_path: &str) -> ExtractionResult {
    let ddl = build_ddl(snap);
    let path = std::path::Path::new(virtual_path);
    let mut result = graphify_extract::ast_extract::extract_file(path, &ddl, "sql");

    // Node ids must match what the extractor produced for the same label.
    let node_id = |schema: &str, name: &str| make_id(&[virtual_path, &qualified(schema, name)]);

    for f in &snap.foreign_keys {
        let mut extra: HashMap<String, Value> = HashMap::new();
        extra.insert(
            "context".to_string(),
            Value::String("foreign_key".to_string()),
        );
        extra.insert(
            "constraint".to_string(),
            Value::String(f.constraint.clone()),
        );
        extra.insert("columns".to_string(), Value::String(f.columns.join(", ")));
        extra.insert(
            "foreign_columns".to_string(),
            Value::String(f.foreign_columns.join(", ")),
        );
        result.edges.push(GraphEdge {
            source: node_id(&f.schema, &f.table),
            target: node_id(&f.foreign_schema, &f.foreign_table),
            relation: "references".to_string(),
            confidence: Confidence::Extracted,
            confidence_score: Confidence::Extracted.default_score(),
            source_file: virtual_path.to_string(),
            source_location: Some("L1".to_string()),
            weight: 1.0,
            provenance: Some("postgres:foreign_key".to_string()),
            extra,
        });
    }

    result
}

/// Build a connection config from a DSN, falling back to `PG*` env vars.
///
/// An empty DSN means "use the environment", matching `psql` behaviour.
fn connection_config(dsn: &str) -> Result<Config> {
    if !dsn.trim().is_empty() {
        // Never include the DSN in the error — it carries the password.
        return dsn.parse::<Config>().map_err(|e| {
            anyhow::anyhow!(
                "invalid PostgreSQL connection string: {}",
                first_line(&e.to_string())
            )
        });
    }

    let mut cfg = Config::new();
    cfg.host(std::env::var("PGHOST").as_deref().unwrap_or("localhost"));
    if let Ok(port) = std::env::var("PGPORT")
        && let Ok(port) = port.parse::<u16>()
    {
        cfg.port(port);
    }
    if let Ok(user) = std::env::var("PGUSER") {
        cfg.user(&user);
    } else if let Ok(user) = std::env::var("USER") {
        cfg.user(&user);
    }
    if let Ok(password) = std::env::var("PGPASSWORD") {
        cfg.password(&password);
    }
    if let Ok(db) = std::env::var("PGDATABASE") {
        cfg.dbname(&db);
    }
    Ok(cfg)
}

/// First line only — driver errors can embed the connection string.
fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").trim().to_string()
}

/// A stable, credential-free label for the database being read.
fn virtual_path_for(cfg: &Config) -> String {
    let host = cfg
        .get_hosts()
        .iter()
        .map(|h| match h {
            tokio_postgres::config::Host::Tcp(name) => name.clone(),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
        })
        .next()
        .unwrap_or_else(|| "localhost".to_string());
    let dbname = cfg.get_dbname().unwrap_or("db");
    format!("postgresql://{host}/{dbname}")
}

/// Connect, read the schema, and return nodes and edges for it.
pub async fn introspect_postgres(dsn: &str) -> Result<ExtractionResult> {
    let cfg = connection_config(dsn)?;
    let virtual_path = virtual_path_for(&cfg);

    let connector = native_tls::TlsConnector::new().context("could not initialise TLS")?;
    let connector = postgres_native_tls::MakeTlsConnector::new(connector);

    let (client, connection) = cfg.connect(connector).await.map_err(|e| {
        anyhow::anyhow!(
            "could not connect to PostgreSQL: {}",
            first_line(&e.to_string())
        )
    })?;

    // The connection future drives the socket; it ends when the client drops.
    let handle = tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!("postgres connection closed: {e}");
        }
    });

    let result = read_schema(&client, &virtual_path).await;
    drop(client);
    let _ = handle.await;
    result
}

async fn read_schema(
    client: &tokio_postgres::Client,
    virtual_path: &str,
) -> Result<ExtractionResult> {
    // Introspection must never be able to write.
    client
        .batch_execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE")
        .await
        .context("could not enter a read-only transaction")?;

    let mut snap = SchemaSnapshot::default();

    for row in client
        .query(
            "SELECT table_schema, table_name, table_type
               FROM information_schema.tables
              WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
              ORDER BY table_schema, table_name",
            &[],
        )
        .await
        .context("could not read tables")?
    {
        snap.tables.push(TableRow {
            schema: row.get(0),
            name: row.get(1),
            kind: row.get::<_, Option<String>>(2).unwrap_or_default(),
        });
    }

    for row in client
        .query(
            "SELECT table_schema, table_name, view_definition
               FROM information_schema.views
              WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
              ORDER BY table_schema, table_name",
            &[],
        )
        .await
        .context("could not read views")?
    {
        snap.views.push(ViewRow {
            schema: row.get(0),
            name: row.get(1),
            body: row.get(2),
        });
    }

    for row in client
        .query(
            "SELECT routine_schema, routine_name, routine_type,
                    routine_definition, external_language
               FROM information_schema.routines
              WHERE routine_schema NOT IN ('pg_catalog', 'information_schema')
              ORDER BY routine_schema, routine_name",
            &[],
        )
        .await
        .context("could not read routines")?
    {
        snap.routines.push(RoutineRow {
            schema: row.get(0),
            name: row.get(1),
            kind: row.get::<_, Option<String>>(2).unwrap_or_default(),
            body: row.get(3),
            language: row.get(4),
        });
    }

    // Read pg_constraint, NOT information_schema.referential_constraints: that
    // view only shows constraints where the current role has write access to
    // the referencing table, so a read-only introspection role sees tables and
    // views but zero foreign keys — the graph would quietly lose every edge.
    // pg_constraint is not privilege-filtered, and keying by oid avoids
    // cross-matching same-named constraints on sibling tables.
    for row in client
        .query(
            "SELECT con.conname,
                    ns.nspname,
                    rel.relname,
                    (SELECT ARRAY_AGG(att.attname ORDER BY k.ord)
                       FROM UNNEST(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                       JOIN pg_catalog.pg_attribute att
                         ON att.attrelid = con.conrelid AND att.attnum = k.attnum),
                    fns.nspname,
                    frel.relname,
                    (SELECT ARRAY_AGG(att.attname ORDER BY k.ord)
                       FROM UNNEST(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                       JOIN pg_catalog.pg_attribute att
                         ON att.attrelid = con.confrelid AND att.attnum = k.attnum)
               FROM pg_catalog.pg_constraint con
               JOIN pg_catalog.pg_class rel ON rel.oid = con.conrelid
               JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace
               JOIN pg_catalog.pg_class frel ON frel.oid = con.confrelid
               JOIN pg_catalog.pg_namespace fns ON fns.oid = frel.relnamespace
              WHERE con.contype = 'f'
                AND ns.nspname NOT IN ('pg_catalog', 'information_schema')
              ORDER BY ns.nspname, rel.relname, con.conname",
            &[],
        )
        .await
        .context("could not read foreign keys")?
    {
        snap.foreign_keys.push(ForeignKeyRow {
            constraint: row.get(0),
            schema: row.get(1),
            table: row.get(2),
            columns: row.get::<_, Option<Vec<String>>>(3).unwrap_or_default(),
            foreign_schema: row.get(4),
            foreign_table: row.get(5),
            foreign_columns: row.get::<_, Option<Vec<String>>>(6).unwrap_or_default(),
        });
    }

    if snap.tables.is_empty() && snap.views.is_empty() && snap.routines.is_empty() {
        bail!("connected to PostgreSQL but found no tables, views, or routines to index");
    }

    Ok(to_extraction(&snap, virtual_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: &str = "postgresql://localhost/shop";

    fn table(schema: &str, name: &str) -> TableRow {
        TableRow {
            schema: schema.into(),
            name: name.into(),
            kind: "BASE TABLE".into(),
        }
    }

    fn sample() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![table("public", "users"), table("public", "orders")],
            views: vec![ViewRow {
                schema: "public".into(),
                name: "active_users".into(),
                body: Some("SELECT * FROM users WHERE active".into()),
            }],
            routines: vec![RoutineRow {
                schema: "public".into(),
                name: "refresh".into(),
                kind: "PROCEDURE".into(),
                body: None,
                language: None,
            }],
            foreign_keys: vec![ForeignKeyRow {
                constraint: "orders_user_fk".into(),
                schema: "public".into(),
                table: "orders".into(),
                columns: vec!["user_id".into()],
                foreign_schema: "public".into(),
                foreign_table: "users".into(),
                foreign_columns: vec!["id".into()],
            }],
        }
    }

    #[test]
    fn quotes_only_identifiers_that_need_it() {
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("user_id2"), "user_id2");
        assert_eq!(quote_ident("MixedCase"), "\"MixedCase\"");
        assert_eq!(quote_ident("has-hyphen"), "\"has-hyphen\"");
        assert_eq!(quote_ident("odd\"name"), "\"odd\"\"name\"");
    }

    #[test]
    fn tables_views_and_routines_become_nodes() {
        let r = to_extraction(&sample(), VP);
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"public.users"));
        assert!(labels.contains(&"public.orders"));
        assert!(labels.contains(&"public.active_users"));
        assert!(labels.contains(&"public.refresh"));
    }

    #[test]
    fn foreign_keys_become_reference_edges_between_real_nodes() {
        let r = to_extraction(&sample(), VP);
        let ids: Vec<&str> = r.nodes.iter().map(|n| n.id.as_str()).collect();
        let fk = r
            .edges
            .iter()
            .find(|e| e.relation == "references")
            .expect("expected a references edge");

        // The edge must attach to nodes the extractor actually created,
        // otherwise graph assembly prunes it as dangling.
        assert!(ids.contains(&fk.source.as_str()), "source is dangling");
        assert!(ids.contains(&fk.target.as_str()), "target is dangling");
        assert_eq!(fk.extra["constraint"], "orders_user_fk");
        assert_eq!(fk.extra["columns"], "user_id");
    }

    #[test]
    fn procedures_are_written_as_functions() {
        let ddl = build_ddl(&sample());
        assert!(ddl.contains("CREATE FUNCTION public.refresh()"));
        assert!(!ddl.contains("CREATE PROCEDURE"));
        // No language in the catalog falls back to plpgsql.
        assert!(ddl.contains("LANGUAGE plpgsql"));
    }

    #[test]
    fn unreadable_view_body_still_yields_a_node() {
        let snap = SchemaSnapshot {
            views: vec![ViewRow {
                schema: "public".into(),
                name: "secret".into(),
                body: None,
            }],
            ..Default::default()
        };
        assert!(build_ddl(&snap).contains("CREATE VIEW public.secret AS SELECT 1;"));
        let r = to_extraction(&snap, VP);
        assert!(r.nodes.iter().any(|n| n.label == "public.secret"));
    }

    #[test]
    fn a_body_cannot_inject_spurious_objects() {
        // A view whose body contains DDL at the start of a line would otherwise
        // be picked up by the line-anchored extractor patterns.
        let snap = SchemaSnapshot {
            views: vec![ViewRow {
                schema: "public".into(),
                name: "v".into(),
                body: Some("SELECT 1;\nCREATE TABLE injected (x INT);".into()),
            }],
            ..Default::default()
        };
        let r = to_extraction(&snap, VP);
        assert!(
            !r.nodes.iter().any(|n| n.label.contains("injected")),
            "body content must not become a node"
        );
    }

    #[test]
    fn composite_foreign_keys_keep_column_order() {
        let snap = SchemaSnapshot {
            tables: vec![table("public", "a"), table("public", "b")],
            foreign_keys: vec![ForeignKeyRow {
                constraint: "composite_fk".into(),
                schema: "public".into(),
                table: "a".into(),
                columns: vec!["x".into(), "y".into()],
                foreign_schema: "public".into(),
                foreign_table: "b".into(),
                foreign_columns: vec!["p".into(), "q".into()],
            }],
            ..Default::default()
        };
        let ddl = build_ddl(&snap);
        assert!(ddl.contains("FOREIGN KEY (x, y) REFERENCES public.b(p, q)"));
        let r = to_extraction(&snap, VP);
        let fk = r.edges.iter().find(|e| e.relation == "references").unwrap();
        assert_eq!(fk.extra["columns"], "x, y");
        assert_eq!(fk.extra["foreign_columns"], "p, q");
    }

    #[test]
    fn non_base_tables_are_skipped() {
        let snap = SchemaSnapshot {
            tables: vec![TableRow {
                schema: "public".into(),
                name: "a_view".into(),
                kind: "VIEW".into(),
            }],
            ..Default::default()
        };
        assert!(!build_ddl(&snap).contains("CREATE TABLE"));
    }

    #[test]
    fn errors_never_echo_the_connection_string() {
        let err = connection_config("postgres://user:hunter2@host/db?bogus=1")
            .expect_err("an unknown parameter should be rejected")
            .to_string();
        assert!(!err.contains("hunter2"), "password leaked: {err}");
    }

    #[test]
    fn virtual_path_carries_no_credentials() {
        let cfg: Config = "postgres://user:hunter2@db.example.com:5432/shop"
            .parse()
            .unwrap();
        let vp = virtual_path_for(&cfg);
        assert_eq!(vp, "postgresql://db.example.com/shop");
        assert!(!vp.contains("hunter2"));
        assert!(!vp.contains("user"));
    }
}
