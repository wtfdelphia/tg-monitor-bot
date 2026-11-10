use anyhow::{Context, Result};

pub fn pg_port() -> Result<u16> {
    match std::env::var("TGM_TEST_PG_PORT") {
        Ok(value) => value.parse().context("TGM_TEST_PG_PORT 必须是有效端口"),
        Err(std::env::VarError::NotPresent) => Ok(55432),
        Err(error) => Err(error).context("读取 TGM_TEST_PG_PORT 失败"),
    }
}
