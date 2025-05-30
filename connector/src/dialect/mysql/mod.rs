use anyhow::anyhow;
use arrow::array::*;
use arrow::datatypes::{DataType, Field, Schema};
use async_trait::async_trait;
use mysql::consts::ColumnType::*;
use mysql::prelude::*;
use mysql::*;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use crate::dialect::Connection;
use crate::utils::{Metadata, RawArrowData};
use crate::utils::{Table, build_tree};
use crate::utils::{Title, TreeNode};

#[derive(Debug, Default)]
pub struct MySqlConnection {
  pub host: String,
  pub port: String,
  pub username: String,
  pub password: String,
  pub database: Option<String>,
}

#[async_trait]
impl Connection for MySqlConnection {
  async fn get_db(&self) -> anyhow::Result<TreeNode> {
    let tables = self.get_tables()?;
    Ok(TreeNode {
      name: self.host.clone(),
      path: self.host.clone(),
      node_type: "root".to_string(),
      schema: None,
      children: Some(build_tree(tables)),
      size: None,
      comment: None,
    })
  }

  async fn all_columns(&self) -> anyhow::Result<Vec<Metadata>> {
    Ok(self._all_columns()?)
  }

  async fn query(&self, sql: &str, _limit: usize, _offset: usize) -> anyhow::Result<RawArrowData> {
    self._query(sql)
  }

  async fn query_all(&self, sql: &str) -> anyhow::Result<RawArrowData> {
    self._query(sql)
  }

  async fn table_row_count(&self, table: &str, r#where: &str) -> anyhow::Result<usize> {
    self._table_row_count(table, r#where)
  }

  async fn show_schema(&self, schema: &str) -> anyhow::Result<RawArrowData> {
    let sql = format!(
      "select * from information_schema.tables where TABLE_SCHEMA='{schema}' order by TABLE_TYPE, TABLE_NAME"
    );
    self.query(&sql, 0, 0).await
  }

  async fn show_column(&self, schema: Option<&str>, table: &str) -> anyhow::Result<RawArrowData> {
    let (db, tbl) = if schema.is_none() && table.contains('.') {
      let parts: Vec<&str> = table.splitn(2, '.').collect();
      (parts[0], parts[1])
    } else {
      ("", table)
    };
    let sql = format!(
      "select * from information_schema.columns where table_schema='{db}' and table_name='{tbl}'"
    );
    log::info!("show columns: {}", &sql);
    self.query(&sql, 0, 0).await
  }

  #[allow(clippy::unused_async)]
  async fn query_count(&self, sql: &str) -> anyhow::Result<usize> {
    let mut conn = self.get_conn()?;
    if let Some(total) = conn.query_first::<usize, _>(sql)? {
      Ok(total)
    } else {
      Err(anyhow::anyhow!("null"))
    }
  }
}

impl MySqlConnection {
  fn new(host: &str, port: &str, username: &str, password: &str) -> Self {
    Self {
      host: host.to_string(),
      port: port.to_string(),
      username: username.to_string(),
      password: password.to_string(),
      database: None,
    }
  }

  fn get_url(&self) -> String {
    format!(
      "mysql://{}:{}@{}:{}/{}",
      self.username,
      self.password,
      self.host,
      self.port,
      self.database.clone().unwrap_or_default(),
    )
  }

  fn get_conn(&self) -> anyhow::Result<PooledConn> {
    let binding = self.get_url();
    let url = binding.as_str();
    let pool = Pool::new(url)?;
    Ok(pool.get_conn()?)
  }

  fn get_schema(&self) -> Vec<Table> {
    vec![]
  }
  pub fn get_tables(&self) -> anyhow::Result<Vec<Table>> {
    let mut conn = self.get_conn()?;

    let sql = r"
    select
      TABLE_SCHEMA as table_schema,
      TABLE_NAME as table_name,
      TABLE_TYPE as table_type,
      if(TABLE_TYPE='BASE TABLE', 'table', 'view') as type,
      CAST(round(((data_length + IFNULL(index_length, 0)) / 1024 / 1024)) AS UNSIGNED)  AS size
    from information_schema.tables
    ";
    let tables = conn.query_map(
      sql,
      |(table_schema, table_name, table_type, r#type, size)| Table {
        db_name: table_schema,
        table_name,
        table_type,
        r#type,
        size: Some(size),
        schema: None,
      },
    )?;
    Ok(tables)
  }

  fn _all_columns(&self) -> anyhow::Result<Vec<Metadata>> {
    let mut conn = self.get_conn()?;
    let sql = "
    SELECT
        table_schema,
        table_name,
        column_name,
        column_type
    FROM information_schema.columns
    -- WHERE table_schema NOT IN ('mysql', 'performance_schema', 'information_schema', 'sys') -- 排除系统库
    ORDER BY table_schema, table_name, ordinal_position;
    ";

    let rows: Vec<(String, String, String, String)> = conn.query(sql)?;

    // 使用 HashMap 按数据库和表名分组列信息
    let mut groups: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for (db, table, col, dtype) in rows {
      groups
        .entry((db, table))
        .or_insert_with(Vec::new)
        .push((col, dtype));
    }
    // 转换为最终结构
    let metadata_list: Vec<Metadata> = groups
      .into_iter()
      .map(|((database, table), columns)| Metadata {
        database,
        table,
        columns,
      })
      .collect();

    Ok(metadata_list)
  }

  fn _query(&self, sql: &str) -> anyhow::Result<RawArrowData> {
    let mut conn = self.get_conn()?;

    let mut result = conn.query_iter(sql)?;
    let columns = result.columns();
    let columns = columns.as_ref();
    let k = columns.len();

    // let stmt = conn.prep(sql)?;
    // let k = stmt.num_columns();
    // let columns = stmt.columns();

    let mut fields = vec![];
    let mut titles = vec![];
    let mut types = vec![];
    for (i, col) in columns.iter().enumerate() {
      let type_ = format!("{:?}", col.column_type());
      let type_ = type_.strip_suffix("MYSQL_TYPE_").unwrap_or(type_.as_str());
      println!("{i}: {:?}, {:?}", col.name_str(), type_);
      titles.push(Title {
        name: col.name_str().to_string(),
        r#type: type_.to_string(),
      });
      types.push(col.column_type());
      let typ = match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_INT24 | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG => DataType::Int64,
        MYSQL_TYPE_DECIMAL
        | MYSQL_TYPE_NEWDECIMAL
        | MYSQL_TYPE_FLOAT
        | MYSQL_TYPE_YEAR
        | MYSQL_TYPE_DOUBLE => DataType::Float64,
        MYSQL_TYPE_DATETIME => DataType::Utf8,
        MYSQL_TYPE_DATE => DataType::Utf8,
        MYSQL_TYPE_BLOB => DataType::Utf8,
        MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => DataType::Utf8,
        _ => DataType::Binary,
      };
      let field = Field::new(col.name_str(), typ, true);
      fields.push(field);
    }
    let mut tables: Vec<Vec<Value>> = (0..k).map(|_| vec![]).collect();
    while let Some(result_set) = result.iter() {
      for row in result_set.flatten() {
        for (i, _col) in row.columns_ref().iter().enumerate() {
          let val = row.get::<Value, _>(i).unwrap();
          tables[i].push(val);
        }
      }
    }

    let mut arrs = vec![];
    for (type_, col) in types.iter().zip(tables) {
      let arr: ArrayRef = match type_ {
        MYSQL_TYPE_TINY | MYSQL_TYPE_INT24 | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG => Arc::new(Int64Array::from(convert_to_i64_arr(&col))),
        MYSQL_TYPE_DECIMAL
        | MYSQL_TYPE_NEWDECIMAL
        | MYSQL_TYPE_FLOAT
        | MYSQL_TYPE_YEAR
        | MYSQL_TYPE_DOUBLE => Arc::new(Float64Array::from(convert_to_f64_arr(&col))),
        MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
          Arc::new(StringArray::from(convert_to_str_arr(&col)))
        }
        MYSQL_TYPE_DATETIME => Arc::new(StringArray::from(convert_to_str_arr(&col))),
        MYSQL_TYPE_DATE => Arc::new(StringArray::from(convert_to_str_arr(&col))),
        MYSQL_TYPE_BLOB => Arc::new(StringArray::from(convert_to_str_arr(&col))),
        _ => Arc::new(StringArray::from(convert_to_str_arr(&col))),
      };

      arrs.push(arr);
    }

    let schema = Schema::new(fields);
    let batch = RecordBatch::try_new(Arc::new(schema), arrs)?;
    Ok(RawArrowData {
      total: batch.num_rows(),
      batch,
      titles: Some(titles.clone()),
      sql: Some(sql.to_string()),
    })
  }

  fn _table_row_count(&self, table: &str, cond: &str) -> anyhow::Result<usize> {
    let mut conn = self.get_conn()?;
    let mut sql = format!("select count(*) from {table}");
    if !cond.is_empty() {
      sql = format!("{sql} where {cond}");
    }
    conn
      .query_first::<usize, _>(&sql)?
      .ok_or_else(|| anyhow!("No value found"))
  }
  fn _sql_row_count(&self, sql: &str) -> anyhow::Result<usize> {
    let mut conn = self.get_conn()?;
    conn
      .query_first::<usize, _>(&sql)?
      .ok_or_else(|| anyhow!("No value found"))
  }
}

fn convert_to_str(unknown_val: &Value) -> Option<String> {
  match unknown_val {
    val @ Value::Bytes(..) => {
      let val = from_value::<Vec<u8>>(val.clone());
      String::from_utf8(val).ok()
    }
    _ => None,
  }
}

fn convert_to_str_arr(values: &[Value]) -> Vec<Option<String>> {
  values.iter().map(convert_to_str).collect()
}

fn convert_to_i64(unknown_val: &Value) -> Option<i64> {
  match unknown_val {
    val @ Value::Int(..) => {
      let val = from_value::<i64>(val.clone());
      Some(val)
    }
    val @ Value::UInt(..) => {
      let val = from_value::<i64>(val.clone());
      Some(val)
    }
    val @ Value::Bytes(..) => {
      let val = from_value::<i64>(val.clone());
      Some(val)
    }
    _ => None,
  }
}

fn convert_to_i64_arr(values: &[Value]) -> Vec<Option<i64>> {
  values.iter().map(convert_to_i64).collect()
}

fn convert_to_i32(unknown_val: &Value) -> Option<i32> {
  match unknown_val {
    val @ Value::Int(..) => {
      let val = from_value::<i32>(val.clone());
      Some(val)
    }
    _ => None,
  }
}

fn convert_to_i32_arr(values: &[Value]) -> Vec<Option<i32>> {
  values.iter().map(convert_to_i32).collect()
}

fn convert_to_u64(unknown_val: &Value) -> Option<u64> {
  match unknown_val {
    val @ Value::UInt(..) => {
      let val = from_value::<u64>(val.clone());
      Some(val)
    }
    val @ Value::Int(..) => {
      let val = from_value::<u64>(val.clone());
      Some(val)
    }
    val @ Value::Bytes(..) => {
      let val = from_value::<u64>(val.clone());
      Some(val)
    }
    _ => None,
  }
}

fn convert_to_u64_arr(values: &[Value]) -> Vec<Option<u64>> {
  values.iter().map(convert_to_u64).collect()
}

fn convert_to_f64(unknown_val: &Value) -> Option<f64> {
  match unknown_val {
    val @ Value::Float(..) => {
      let val = from_value::<f64>(val.clone());
      Some(val)
    }
    val @ Value::Double(..) => {
      let val = from_value::<f64>(val.clone());
      Some(val)
    }
    val @ Value::Bytes(..) => {
      let val = from_value::<f64>(val.clone());
      Some(val)
    }
    _ => None,
  }
}

fn convert_to_f64_arr(values: &[Value]) -> Vec<Option<f64>> {
  values.iter().map(convert_to_f64).collect()
}

#[tokio::test]
async fn test_query() {}
