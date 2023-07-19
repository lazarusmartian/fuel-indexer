use crate::{IndexerResult, Manifest};
use fuel_indexer_database::{queries, IndexerConnection, IndexerConnectionPool};
use fuel_indexer_lib::{fully_qualified_namespace, graphql::types::IdCol};
use fuel_indexer_schema::FtColumn;
use std::collections::HashMap;
use tracing::{debug, error, info};

/// Database for an executor instance, with schema info.
#[derive(Debug)]
pub struct Database {
    pool: IndexerConnectionPool,
    stashed: Option<IndexerConnection>,
    pub namespace: String,
    pub identifier: String,
    pub version: String,
    pub schema: HashMap<String, Vec<String>>,
    pub tables: HashMap<i64, String>,
}

// TODO: Use mutex
unsafe impl Sync for Database {}
unsafe impl Send for Database {}

fn is_id_only_upsert(columns: &[String]) -> bool {
    columns.len() == 2 && columns[0] == IdCol::to_lowercase_string()
}

impl Database {
    /// Create a new `Database`.
    pub async fn new(pool: IndexerConnectionPool, manifest: &Manifest) -> Database {
        Database {
            pool,
            stashed: None,
            namespace: manifest.namespace.clone(),
            identifier: manifest.identifier.clone(),
            version: Default::default(),
            schema: Default::default(),
            tables: Default::default(),
        }
    }

    /// Open a database transaction.
    pub async fn start_transaction(&mut self) -> IndexerResult<usize> {
        let conn = self.pool.acquire().await?;
        self.stashed = Some(conn);
        debug!("Connection stashed as: {:?}", self.stashed);
        let conn = self.stashed.as_mut().expect(
            "No stashed connection for start transaction. Was a transaction started?",
        );
        let result = queries::start_transaction(conn).await?;
        Ok(result)
    }

    /// Commit transaction to database.
    pub async fn commit_transaction(&mut self) -> IndexerResult<usize> {
        let conn = self
            .stashed
            .as_mut()
            .expect("No stashed connection for commit. Was a transaction started?");
        let res = queries::commit_transaction(conn).await?;
        Ok(res)
    }

    /// Revert open transaction.
    pub async fn revert_transaction(&mut self) -> IndexerResult<usize> {
        let conn = self
            .stashed
            .as_mut()
            .expect("No stashed connection for revert. Was a transaction started?");
        let res = queries::revert_transaction(conn).await?;
        Ok(res)
    }

    /// Build an upsert query using a set of columns, insert values, update values, and a table name.
    fn upsert_query(
        &self,
        table: &str,
        columns: &[String],
        inserts: Vec<String>,
        updates: Vec<String>,
    ) -> String {
        if is_id_only_upsert(columns) {
            format!(
                "INSERT INTO {}
                    ({})
                 VALUES
                    ({}, $1::bytea)
                 ON CONFLICT(id)
                 DO NOTHING",
                table,
                columns.join(", "),
                inserts.join(", "),
            )
        } else {
            format!(
                "INSERT INTO {}
                    ({})
                 VALUES
                    ({}, $1::bytea)
                 ON CONFLICT(id)
                 DO UPDATE SET {}",
                table,
                columns.join(", "),
                inserts.join(", "),
                updates.join(", "),
            )
        }
    }

    /// Return a query to get an object from the database.
    fn get_query(&self, table: &str, object_id: u64) -> String {
        format!("SELECT object from {table} where id = {object_id}")
    }

    /// Put an object into the database.
    pub async fn put_object(
        &mut self,
        type_id: i64,
        columns: Vec<FtColumn>,
        bytes: Vec<u8>,
    ) {
        let table = match self.tables.get(&type_id) {
            Some(t) => t,
            None => {
                error!(
                    r#"TypeId({}) not found in tables: {:?}. 

Does the schema version in SchemaManager::new_schema match the schema version in Database::load_schema?

Do your WASM modules need to be rebuilt?

"#,
                    type_id, self.tables,
                );
                return;
            }
        };

        let inserts: Vec<_> = columns.iter().map(|col| col.query_fragment()).collect();
        let updates: Vec<_> = self.schema[table]
            .iter()
            .zip(columns.iter())
            .map(|(colname, value)| format!("{} = {}", colname, value.query_fragment()))
            .collect();

        let columns = self.schema[table].clone();

        let query_text = self.upsert_query(table, &columns, inserts, updates);

        let conn = self
            .stashed
            .as_mut()
            .expect("No stashed connection for put. Was a transaction started?");

        if let Err(e) = queries::put_object(conn, query_text, bytes).await {
            error!("Failed to put object: {:?}", e);
        }
    }

    /// Get an object from the database.
    pub async fn get_object(&mut self, type_id: i64, object_id: u64) -> Option<Vec<u8>> {
        let table = &self.tables[&type_id];
        let query = self.get_query(table, object_id);
        let conn = self
            .stashed
            .as_mut()
            .expect("No stashed connection for get. Was a transaction started?");

        match queries::get_object(conn, query).await {
            Ok(v) => Some(v),
            Err(e) => {
                if let sqlx::Error::RowNotFound = e {
                    debug!("Row not found for object ID: {object_id}");
                } else {
                    error!("Failed to get object: {:?}", e);
                }
                None
            }
        }
    }

    /// Load the schema for this indexer from the database, and build a mapping of `TypeId`s to
    /// tables.
    pub async fn load_schema(&mut self, version: String) -> IndexerResult<()> {
        self.version = version;

        info!(
            "Loading schema for Indexer({}.{}) with Version({}).",
            self.namespace, self.identifier, self.version
        );

        let mut conn = self.pool.acquire().await?;
        let columns = queries::columns_get_schema(
            &mut conn,
            &self.namespace,
            &self.identifier,
            &self.version,
        )
        .await?;

        for column in columns {
            let table = &format!(
                "{}.{}",
                fully_qualified_namespace(&self.namespace, &self.identifier),
                &column.table_name
            );

            self.tables
                .entry(column.type_id)
                .or_insert_with(|| table.to_string());

            let columns = self
                .schema
                .entry(table.to_string())
                .or_insert_with(Vec::new);

            columns.push(column.column_name);
        }

        Ok(())
    }
}
