use objects::object::AnnotationDecimal;

use super::*;

fn id(n: u8) -> ContentHash {
    ContentHash::from_bytes([n; 32])
}
fn scope(n: u8) -> CollaborationScope {
    CollaborationScope {
        spool: uuid::Uuid::from_u128(1),
        thread: Some(id(n)),
    }
}
fn budget() -> MapBudget {
    MapBudget::new(128, 1_000_000, 128, 1_000_000)
}
fn database() -> Connection {
    let db = Connection::open_in_memory().expect("memory database");
    initialize_schema(&db).expect("projection schema");
    db
}

#[test]
fn fork_shares_root_and_moves_one_target_without_rewriting_referrers() {
    let mut db = database();
    let parent = scope(1);
    let child = scope(2);
    let tx = db.transaction().expect("transaction");
    let mut root = None;
    for target in 10..110 {
        root = SourceTargetMap::update(
            &mut MapStore::new(&tx),
            root,
            id(target),
            Some(id(200)),
            &mut budget(),
        )
        .expect("target projection");
    }
    publish_root(&tx, &parent, None, root).expect("parent root");
    // Model 1,000 referring record projections. Capture must not touch any.
    tx.execute_batch("CREATE TABLE referrers(record INTEGER PRIMARY KEY,target BLOB NOT NULL);
        CREATE TABLE referrer_writes(n INTEGER NOT NULL); INSERT INTO referrer_writes VALUES(0);
        CREATE TRIGGER detect_referrer_update AFTER UPDATE ON referrers BEGIN UPDATE referrer_writes SET n=n+1; END;
        CREATE TRIGGER detect_referrer_delete AFTER DELETE ON referrers BEGIN UPDATE referrer_writes SET n=n+1; END;
        CREATE TRIGGER detect_referrer_insert AFTER INSERT ON referrers BEGIN UPDATE referrer_writes SET n=n+1; END;")
        .expect("referrer write guard");
    for n in 0..1_000 {
        tx.execute(
            "INSERT INTO referrers VALUES(?1,?2)",
            params![n, id(10).as_bytes()],
        )
        .expect("referrer");
    }
    tx.execute("UPDATE referrer_writes SET n=0", [])
        .expect("reset measured setup writes");
    let before = tx.total_changes();
    fork(&tx, &parent, &child).expect("fork root");
    assert_eq!(
        tx.total_changes() - before,
        1,
        "fork copies only one root row"
    );
    let before = tx.total_changes();
    let changed = SourceTargetMap::update(
        &mut MapStore::new(&tx),
        root,
        id(10),
        Some(id(201)),
        &mut budget(),
    )
    .expect("move target");
    publish_root(&tx, &child, Some(root), changed).expect("publish child");
    assert!(
        tx.total_changes() - before <= 54,
        "writes bounded by trie route, not 1000 referrers"
    );
    let inherited = SourceTargetReference {
        target: id(10),
        binding: SourceTargetBinding::ViewedThread,
    };
    let named = SourceTargetReference {
        target: id(10),
        binding: SourceTargetBinding::NamedThread {
            scope: parent.clone(),
        },
    };
    assert_eq!(
        resolve(&tx, &child, &inherited, &mut budget()).expect("child"),
        Some(id(201))
    );
    assert_eq!(
        resolve(&tx, &parent, &inherited, &mut budget()).expect("parent"),
        Some(id(200))
    );
    assert_eq!(
        resolve(&tx, &child, &named, &mut budget()).expect("named parent"),
        Some(id(200))
    );
    let writes: i64 = tx
        .query_row("SELECT n FROM referrer_writes", [], |r| r.get(0))
        .expect("write count");
    assert_eq!(writes, 0, "no referring rows rewritten");
    assert!(matches!(
        publish_root(&tx, &child, Some(root), root),
        Err(Error::Stale)
    ));
    tx.commit().expect("commit");
}

#[test]
fn query_keeps_types_scope_and_exact_decimal_values_separate() {
    let mut db = database();
    let tx = db.transaction().expect("transaction");
    let values = [
        AnnotationValue::Integer(1),
        AnnotationValue::Text("1".into()),
        AnnotationValue::Boolean(true),
        AnnotationValue::Decimal(AnnotationDecimal {
            coefficient: 1,
            scale: 0,
        }),
    ];
    for (index, value) in values.iter().enumerate() {
        put_property(&tx, &scope(1), id(index as u8), "priority", value).expect("property");
        put_property(&tx, &scope(2), id(index as u8), "priority", value).expect("other Thread");
    }
    for (index, value) in values.iter().enumerate() {
        assert_eq!(
            property_equal(&tx, &scope(1), "priority", value, None, 10).expect("query"),
            vec![id(index as u8)]
        );
        assert!(
            property_equal(&tx, &scope(1), "priority", value, Some(id(index as u8)), 10)
                .expect("next page")
                .is_empty()
        );
    }
    // Preserve distinctions beyond f64's exact integer range.
    for (n, coefficient) in [(10, 9_007_199_254_740_992), (11, 9_007_199_254_740_993)] {
        let value = AnnotationValue::Decimal(AnnotationDecimal {
            coefficient,
            scale: 0,
        });
        put_property(&tx, &scope(1), id(n), "confidence", &value).expect("exact decimal");
        assert_eq!(
            property_equal(&tx, &scope(1), "confidence", &value, None, 10).expect("exact query"),
            vec![id(n)]
        );
    }
    assert!(
        value_key(&AnnotationValue::Integer(-2)).expect("key").1
            < value_key(&AnnotationValue::Integer(-1)).expect("key").1
    );
    assert!(property_equal(&tx, &scope(1), "priority", &values[0], None, 0).is_err());
    tx.commit().expect("commit");
}

#[test]
fn failed_publication_rolls_back_and_bounded_reads_refuse_before_body_return() {
    let mut db = database();
    {
        let tx = db.transaction().expect("transaction");
        let root = SourceTargetMap::update(
            &mut MapStore::new(&tx),
            None,
            id(1),
            Some(id(2)),
            &mut budget(),
        )
        .expect("map");
        publish_root(&tx, &scope(1), None, root).expect("root");
        let root_hash = root.expect("nonempty map");
        assert!(matches!(
            MapStore::new(&tx).read(root_hash, 1),
            Err(Error::Invalid("map node exceeds read budget"))
        ));
        assert!(matches!(
            publish_root(&tx, &scope(2), None, Some(id(250))),
            Err(Error::Invalid("root node is missing"))
        ));
    }
    assert_eq!(root(&db, &scope(1)).expect("rolled back root"), None);
    let count: i64 = db
        .query_row("SELECT count(*) FROM reference_map_nodes", [], |r| r.get(0))
        .expect("nodes");
    assert_eq!(count, 0);
}

#[test]
fn replacing_tags_removes_stale_properties_and_rejects_duplicate_keys_before_writing() {
    let mut db = database();
    let tx = db.transaction().expect("transaction");
    let tag = AnnotationTag::Property {
        key: "severity".into(),
        value: AnnotationValue::Text("high".into()),
    };
    project_properties(&tx, &scope(1), id(7), &[tag.clone()]).expect("project complete head");
    assert_eq!(
        property_equal(
            &tx,
            &scope(1),
            "severity",
            &AnnotationValue::Text("high".into()),
            None,
            5
        )
        .expect("query"),
        vec![id(7)]
    );
    let before = tx.total_changes();
    assert!(project_properties(&tx, &scope(1), id(7), &[tag.clone(), tag]).is_err());
    assert_eq!(
        tx.total_changes(),
        before,
        "canonical validation precedes mutation"
    );
    project_properties(&tx, &scope(1), id(7), &[]).expect("remove superseded head");
    assert!(
        property_equal(
            &tx,
            &scope(1),
            "severity",
            &AnnotationValue::Text("high".into()),
            None,
            5
        )
        .expect("query")
        .is_empty()
    );
    let details: Vec<String> = tx.prepare("EXPLAIN QUERY PLAN SELECT record FROM annotation_property_index
        WHERE spool=?1 AND thread=?2 AND name=?3 AND kind=?4 AND value=?5 AND record>?6 ORDER BY record LIMIT ?7")
        .expect("query plan").query_map(params![scope(1).spool.as_bytes(), id(1).as_bytes(), "severity", 0, b"high", b"", 5], |row| row.get(3))
        .expect("explain").collect::<Result<_,_>>().expect("plan rows");
    assert!(
        details
            .iter()
            .any(|line| line.contains("USING COVERING INDEX annotation_property_lookup")),
        "{details:?}"
    );
}
