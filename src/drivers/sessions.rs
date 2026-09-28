//! Server sessions for the process list: each engine's catalog query (the
//! first column is the id [`signal_sql`] takes) and how it cancels a
//! session's statement or ends the session.

use crate::engine::{Dialect, Engine};

const POSTGRES: &str = "SELECT pid AS id,
        usename AS user,
        datname AS database,
        COALESCE(client_addr::text, 'local') AS client,
        application_name AS application,
        state,
        CASE WHEN state = 'active' THEN to_char(now() - query_start, 'HH24:MI:SS') END AS running_for,
        wait_event_type AS waiting_on,
        query
   FROM pg_stat_activity
  WHERE backend_type = 'client backend'
  ORDER BY state = 'active' DESC, query_start DESC NULLS LAST";

const COCKROACH: &str = "SELECT session_id AS id, user_name AS user, client_address AS client,
        application_name AS application, status AS state, active_queries AS query, session_start
   FROM crdb_internal.cluster_sessions
  ORDER BY session_start DESC";

const REDSHIFT: &str = "SELECT pid AS id, user_name AS user, db_name AS database, status AS state,
        starttime AS started, duration AS microseconds, query
   FROM stv_recents WHERE status = 'Running' ORDER BY starttime";

const VERTICA: &str = "SELECT session_id AS id, user_name AS user, client_hostname AS client,
        client_label AS application, statement_start AS started, current_statement AS query
   FROM v_monitor.sessions ORDER BY statement_start DESC";

const MYSQL: &str =
    "SELECT ID AS id, USER AS user, DB AS `database`, HOST AS client, COMMAND AS command,
        TIME AS seconds, STATE AS state, INFO AS query
   FROM information_schema.PROCESSLIST
  ORDER BY COMMAND = 'Sleep', TIME DESC";

const MSSQL: &str =
    "SELECT s.session_id AS id, s.login_name AS [user], DB_NAME(s.database_id) AS [database],
        s.host_name AS client, s.program_name AS application, COALESCE(r.status, s.status) AS state,
        r.total_elapsed_time / 1000 AS seconds, r.wait_type AS waiting_on, t.text AS query
   FROM sys.dm_exec_sessions s
   LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
   OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
  WHERE s.is_user_process = 1
  ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, r.total_elapsed_time DESC";

const ORACLE: &str =
    "SELECT s.sid || ',' || s.serial# AS \"id\", s.username AS \"user\", s.machine AS \"client\",
        s.program AS \"application\", s.status AS \"state\", s.last_call_et AS \"seconds\",
        s.event AS \"waiting_on\", q.sql_text AS \"query\"
   FROM v$session s LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = 0
  WHERE s.type = 'USER'
  ORDER BY s.status, s.last_call_et DESC";

const CLICKHOUSE: &str =
    "SELECT query_id AS id, user, address AS client, round(elapsed, 1) AS seconds,
        read_rows, memory_usage, query
   FROM system.processes ORDER BY elapsed DESC";

const SNOWFLAKE: &str =
    "SELECT query_id AS \"id\", user_name AS \"user\", warehouse_name AS \"warehouse\",
        execution_status AS \"state\", start_time AS \"started\", query_text AS \"query\"
   FROM TABLE(information_schema.query_history(result_limit => 1000))
  WHERE execution_status IN ('RUNNING', 'QUEUED', 'BLOCKED', 'RESUMING_WAREHOUSE')
  ORDER BY start_time DESC";

/// The engine's session list statement (Redis / MongoDB list theirs in
/// their drivers); `None` for embedded engines and those without one.
pub fn list_sql(e: Engine) -> Option<&'static str> {
    Some(match e {
        Engine::Postgres | Engine::Greenplum => POSTGRES,
        Engine::Cockroach => COCKROACH,
        Engine::Redshift => REDSHIFT,
        Engine::Vertica => VERTICA,
        Engine::MySql | Engine::MariaDb => MYSQL,
        Engine::MsSql => MSSQL,
        Engine::Oracle => ORACLE,
        Engine::ClickHouse => CLICKHOUSE,
        Engine::Snowflake => SNOWFLAKE,
        _ => return None,
    })
}

/// The statement that cancels session `id`'s running statement (`kill`:
/// ends the session). Ids come from the list, so only their own
/// characters are allowed in.
pub fn signal_sql(e: Engine, id: &str, kill: bool) -> Result<String, String> {
    let id = id.trim();
    let numeric = || {
        id.parse::<i64>()
            .map_err(|_| format!("'{id}' isn't a session id"))
    };
    let lit = |d: Dialect| d.literal(id);
    Ok(match (e, kill) {
        (Engine::Postgres | Engine::Greenplum, false) => {
            format!("SELECT pg_cancel_backend({})", numeric()?)
        }
        (Engine::Postgres | Engine::Greenplum | Engine::Redshift, true) => {
            format!("SELECT pg_terminate_backend({})", numeric()?)
        }
        (Engine::Redshift, false) => format!("CANCEL {}", numeric()?),
        (Engine::Cockroach, false) => format!(
            "CANCEL QUERIES (SELECT query_id FROM [SHOW CLUSTER QUERIES] WHERE session_id = {})",
            lit(Dialect::Postgres)
        ),
        (Engine::Cockroach, true) => format!("CANCEL SESSION {}", lit(Dialect::Postgres)),
        (Engine::Vertica, false) => format!(
            "SELECT INTERRUPT_STATEMENT(session_id, statement_id) FROM v_monitor.sessions WHERE session_id = {}",
            lit(Dialect::Postgres)
        ),
        (Engine::Vertica, true) => format!("SELECT CLOSE_SESSION({})", lit(Dialect::Postgres)),
        (Engine::MySql | Engine::MariaDb, false) => format!("KILL QUERY {}", numeric()?),
        (Engine::MySql | Engine::MariaDb | Engine::MsSql, true) => format!("KILL {}", numeric()?),
        (Engine::MsSql, false) => return Err("SQL Server can only kill the whole session.".into()),
        (Engine::Oracle, _) => {
            if !id.chars().all(|c| c.is_ascii_digit() || c == ',') {
                return Err(format!("'{id}' isn't a session id"));
            }
            if kill {
                format!("ALTER SYSTEM KILL SESSION '{id}' IMMEDIATE")
            } else {
                format!("ALTER SYSTEM CANCEL SQL '{id}'")
            }
        }
        (Engine::ClickHouse, _) => format!(
            "KILL QUERY WHERE query_id = {} ASYNC",
            lit(Dialect::ClickHouse)
        ),
        (Engine::Snowflake, _) => {
            format!("SELECT SYSTEM$CANCEL_QUERY({})", lit(Dialect::Snowflake))
        }
        _ => return Err(format!("{} has no sessions to signal.", e.label())),
    })
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;

    #[test]
    fn signals() {
        assert_eq!(
            super::signal_sql(Engine::MySql, "12", false).unwrap(),
            "KILL QUERY 12"
        );
        assert_eq!(
            super::signal_sql(Engine::MsSql, "53", true).unwrap(),
            "KILL 53"
        );
        assert!(super::signal_sql(Engine::MySql, "1; DROP", true).is_err());
        assert!(super::signal_sql(Engine::Oracle, "1,2' --", true).is_err());
        assert_eq!(
            super::signal_sql(Engine::ClickHouse, "a'b", false).unwrap(),
            "KILL QUERY WHERE query_id = 'a''b' ASYNC"
        );
    }
}

/// Session lists on the local containers; MySQL and Redis also end a second
/// connection of the test's own.
#[cfg(test)]
mod live_tests {
    use crate::drivers::live;
    use crate::engine::Engine as E;

    #[test]
    fn live_session_lists() {
        let rt = crate::db::runtime();
        for (engine, port, user, db, pass) in [
            (E::MySql, 33306, "root", "shop", "tusk"),
            (E::MariaDb, 33307, "root", "shop", "tusk"),
            (E::MsSql, 31433, "sa", "master", "Tusk_pass123"),
            (E::ClickHouse, 38123, "default", "shop", "tusk"),
            (E::Cockroach, 26257, "root", "shop", ""),
            (E::Oracle, 31521, "system", "FREEPDB1", "tusk"),
            (E::Vertica, 35433, "dbadmin", "docker", ""),
            (E::Redis, 36379, "", "0", ""),
            (E::MongoDb, 37017, "", "admin", ""),
        ] {
            if !live::reachable(port) {
                continue;
            }
            let c = live::conn(engine, port, user, db);
            let a = rt
                .block_on(crate::drivers::connect(
                    &c,
                    c.host.clone(),
                    c.port,
                    pass.into(),
                ))
                .unwrap();
            let rows = rt
                .block_on(a.driver().sessions())
                .unwrap_or_else(|e| panic!("{}: {e}", engine.label()));
            // ClickHouse lists running queries: its own list query at least.
            assert!(
                !rows.is_empty() || engine == E::MongoDb,
                "{}: no sessions",
                engine.label()
            );
            if let Some(first) = rows.first() {
                assert_eq!(
                    first.as_object().unwrap().keys().next().map(String::as_str),
                    Some("id"),
                    "{}",
                    engine.label()
                );
            }
        }
        // Kill a second connection.
        if live::reachable(33306) {
            let c = live::conn(E::MySql, 33306, "root", "shop");
            let a = rt
                .block_on(crate::drivers::connect(
                    &c,
                    c.host.clone(),
                    c.port,
                    "tusk".into(),
                ))
                .unwrap();
            let b = rt
                .block_on(crate::drivers::connect(
                    &c,
                    c.host.clone(),
                    c.port,
                    "tusk".into(),
                ))
                .unwrap();
            let id = rt
                .block_on(
                    b.driver()
                        .query_rows("SELECT CONNECTION_ID() AS id".into(), 1),
                )
                .unwrap()[0]["id"]
                .as_i64()
                .unwrap();
            let listed = rt.block_on(a.driver().sessions()).unwrap();
            assert!(listed.iter().any(|r| r["id"].as_i64() == Some(id)));
            rt.block_on(a.driver().signal_session(id.to_string(), true))
                .unwrap();
            let after = rt.block_on(a.driver().sessions()).unwrap();
            assert!(
                !after.iter().any(|r| r["id"].as_i64() == Some(id)),
                "MySQL session {id} still there"
            );
        }
        if live::reachable(36379) {
            let c = live::conn(E::Redis, 36379, "", "0");
            let a = rt
                .block_on(crate::drivers::connect(
                    &c,
                    c.host.clone(),
                    c.port,
                    String::new(),
                ))
                .unwrap();
            let b = rt
                .block_on(crate::drivers::connect(
                    &c,
                    c.host.clone(),
                    c.port,
                    String::new(),
                ))
                .unwrap();
            let r = rt
                .block_on(b.driver().query_rows("CLIENT ID".into(), 1))
                .unwrap();
            let id = r[0]["result"].as_i64().unwrap();
            rt.block_on(a.driver().signal_session(id.to_string(), true))
                .unwrap();
            let after = rt.block_on(a.driver().sessions()).unwrap();
            assert!(
                !after.iter().any(|r| r["id"].as_i64() == Some(id)),
                "Redis client {id} still there"
            );
            assert!(
                rt.block_on(a.driver().signal_session(id.to_string(), false))
                    .is_err()
            );
        }
    }
}
