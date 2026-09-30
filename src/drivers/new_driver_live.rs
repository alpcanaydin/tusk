//! Required in the dedicated CI job; local runs opt in explicitly.
use super::*;
use serde_json::json;
fn enabled() -> bool {
    std::env::var_os("TUSK_TEST_NEW_DRIVERS").is_some()
}
#[test]
fn trino_new_driver_live() {
    if !enabled() {
        eprintln!("skip: set TUSK_TEST_NEW_DRIVERS=1 with the integration compose fixtures");
        return;
    }
    crate::db::runtime().block_on(async {
        let mut c = crate::db::dev_default();
        c.engine = Engine::Trino;
        c.database = "postgres".into();
        c.user = "tusk".into();
        c.ssl = crate::db::SslMode::Disable;
        c.options.insert("schema".into(), "public".into());
        let db = trino::connect(&c, "127.0.0.1".into(), 58080, String::new())
            .await
            .expect("required Trino fixture unavailable");
        let d = db.driver();
        assert!(d.databases().await.unwrap().contains(&"postgres".into()));
        assert!(d.schemas().await.unwrap().contains(&"public".into()));
        let table = format!("tusk_trino_{}", std::process::id());
        let target = format!("postgres.public.{table}");
        d.exec(format!("DROP TABLE IF EXISTS {target}"))
            .await
            .unwrap();
        d.exec(format!("CREATE TABLE {target} (id BIGINT, name VARCHAR)"))
            .await
            .unwrap();
        assert_eq!(
            d.exec(format!(
                "INSERT INTO {target} VALUES (1, 'first'), (2, 'second')"
            ))
            .await
            .unwrap(),
            2
        );
        assert_eq!(
            d.count("public".into(), table.clone(), None).await.unwrap(),
            2
        );
        assert_eq!(
            d.columns("public".into(), table.clone())
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            d.objects("public".into())
                .await
                .unwrap()
                .tables
                .contains(&table)
        );
        let rows = d
            .query_rows(format!("SELECT * FROM {target} ORDER BY id"), 1)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "first");
        assert!(
            d.query_rows("SELECT * FROM missing_catalog.missing.table".into(), 1)
                .await
                .is_err()
        );
        assert!(
            d.exec("INSERT INTO tpch.tiny.nation VALUES (1, 'x', 1, 'x')".into())
                .await
                .is_err()
        );
        d.exec(format!("DELETE FROM {target} WHERE id=1"))
            .await
            .unwrap();
        assert_eq!(d.count("public".into(), table, None).await.unwrap(), 1);
        d.exec(format!("DROP TABLE {target}")).await.unwrap();
    });
}
#[test]
fn elasticsearch_new_driver_live() {
    if !enabled() {
        eprintln!("skip: set TUSK_TEST_NEW_DRIVERS=1 with the integration compose fixtures");
        return;
    }
    crate::db::runtime().block_on(async {
        let index = format!("tusk-es-{}", std::process::id());
        let base = "http://127.0.0.1:59200";
        let client = http::client();
        let _ = client.delete(format!("{base}/{index}")).send().await;
        client
            .put(format!("{base}/{index}"))
            .json(&json!({}))
            .send()
            .await
            .expect("required Elasticsearch fixture unavailable")
            .error_for_status()
            .unwrap();
        let mut c = crate::db::dev_default();
        c.engine = Engine::Elasticsearch;
        c.database = index.clone();
        c.user = String::new();
        c.ssl = crate::db::SslMode::Disable;
        let db = elasticsearch::connect(&c, "127.0.0.1".into(), 59200, String::new())
            .await
            .unwrap();
        let d = db.driver();
        let created = d
            .document_write(
                index.clone(),
                "one".into(),
                Some(json!({"nested":{"name":"café"},"n":1})),
                None,
            )
            .await
            .unwrap();
        let guard = created["_seq_no"]
            .as_u64()
            .zip(created["_primary_term"].as_u64())
            .unwrap();
        assert!(
            d.document_write(index.clone(), "one".into(), Some(json!({})), None)
                .await
                .is_err()
        );
        let loaded = d.document_get(index.clone(), "one".into()).await.unwrap();
        assert_eq!(loaded["_source"]["nested"]["name"], "café");
        let updated = d
            .document_write(
                index.clone(),
                "one".into(),
                Some(json!({"n":2})),
                Some(guard),
            )
            .await
            .unwrap();
        assert!(
            d.document_write(index.clone(), "one".into(), None, Some(guard))
                .await
                .is_err()
        );
        let guard2 = updated["_seq_no"]
            .as_u64()
            .zip(updated["_primary_term"].as_u64())
            .unwrap();
        d.document_write(index.clone(), "one".into(), None, Some(guard2))
            .await
            .unwrap();
        let mut bulk = String::new();
        for n in 0..10050 {
            bulk.push_str(&format!(
                "{{\"index\":{{\"_id\":\"{n}\"}}}}\n{{\"n\":{n}}}\n"
            ));
        }
        let response: Value = client
            .post(format!("{base}/{index}/_bulk?refresh=true"))
            .header("Content-Type", "application/x-ndjson")
            .body(bulk)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["errors"], false);
        assert_eq!(
            d.count("indices".into(), index.clone(), None)
                .await
                .unwrap(),
            10050
        );
        assert_eq!(
            d.query_rows("{\"query\":{\"match_all\":{}}}".into(), 10050)
                .await
                .unwrap()
                .len(),
            10050
        );
        assert!(d.query_rows("not json".into(), 1).await.is_err());
        assert!(
            d.columns("indices".into(), index.clone())
                .await
                .unwrap()
                .iter()
                .any(|c| c.name == "_source.n")
        );
        let window = |offset| WindowReq {
            schema: "indices".into(),
            table: index.clone(),
            filter: None,
            order_by: None,
            with_key: false,
            limit: 2,
            offset,
        };
        let first = d.window(window(0)).await.unwrap();
        let second = d.window(window(2)).await.unwrap();
        assert_ne!(first[0]["_id"], second[0]["_id"]);
        assert_eq!(d.window(window(0)).await.unwrap(), first);
        // Evicted result pages are refetched using their saved PIT cursor.
        for offset in (4..24).step_by(2) {
            d.window(window(offset)).await.unwrap();
        }
        assert_eq!(d.window(window(0)).await.unwrap(), first);
        assert_eq!(d.window(window(2)).await.unwrap(), second);
        let raw = d
            .query_rows("{\"aggs\":{\"n\":{\"max\":{\"field\":\"n\"}}}}".into(), 1)
            .await
            .unwrap();
        assert!(raw[0].get("aggregations").is_some());
        client
            .post(format!("{base}/_aliases"))
            .json(&json!({"actions":[{"add":{"index":index,"alias":"tusk-es-test-alias"}}]}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(
            d.document_get("tusk-es-test-alias".into(), "1".into())
                .await
                .is_err()
        );
        client
            .delete(format!("{base}/{index}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    });
}

#[test]
fn elasticsearch_auth_new_driver_live() {
    if !enabled() {
        return;
    }
    crate::db::runtime().block_on(async {
        let base = "http://127.0.0.1:59201";
        let client = http::client();
        let mut c = crate::db::dev_default();
        c.engine = Engine::Elasticsearch;
        c.database = "tusk-auth-fixture".into();
        c.user = "elastic".into();
        c.ssl = crate::db::SslMode::Disable;
        c.options.insert("auth_mode".into(), "basic".into());
        assert!(elasticsearch::connect(&c, "127.0.0.1".into(), 59201, "wrong".into()).await.is_err());
        let db = elasticsearch::connect(&c, "127.0.0.1".into(), 59201, "tusk".into()).await.expect("required authenticated Elasticsearch fixture unavailable");
        let response = client.post(format!("{base}/_security/api_key"))
            .basic_auth("elastic", Some("tusk"))
            .json(&json!({"name":"tusk-fixture", "role_descriptors":{"reader":{"cluster":["monitor"],"indices":[{"names":[c.database],"privileges":["read","view_index_metadata"]}]}}}))
            .send().await.unwrap().error_for_status().unwrap().json::<serde_json::Value>().await.unwrap();
        let key = response["encoded"].as_str().unwrap().to_string();
        let _ = client.delete(format!("{base}/{}", c.database)).basic_auth("elastic",Some("tusk")).send().await;
        client.put(format!("{base}/{}", c.database)).basic_auth("elastic",Some("tusk")).json(&json!({})).send().await.unwrap().error_for_status().unwrap();
        db.driver().document_write(c.database.clone(), "one".into(), Some(json!({"n":1})), None).await.unwrap();
        c.options.insert("auth_mode".into(), "api_key".into());
        let reader = elasticsearch::connect(&c, "127.0.0.1".into(), 59201, key).await.unwrap();
        let document = reader.driver().document_get(c.database.clone(), "one".into()).await.unwrap();
        assert_eq!(document["_source"]["n"], 1);
        assert_eq!(reader.driver().query_rows("{}".into(), 10).await.unwrap().len(), 1);
        assert!(reader.driver().document_write(c.database.clone(), "two".into(), Some(json!({"n":2})), None).await.is_err());
        client.delete(format!("{base}/{}", c.database)).basic_auth("elastic",Some("tusk")).send().await.unwrap().error_for_status().unwrap();
        client.delete(format!("{base}/_security/api_key")).basic_auth("elastic",Some("tusk")).json(&json!({"ids":[response["id"]]})).send().await.unwrap().error_for_status().unwrap();
    });
}
