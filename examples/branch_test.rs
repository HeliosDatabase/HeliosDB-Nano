use heliosdb_lite::EmbeddedDatabase;

fn main() {
    // Enable tracing
    std::env::set_var("RUST_LOG", "heliosdb_lite::storage::engine=debug");
    tracing_subscriber::fmt::init();
    
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create DB");
    
    db.execute("CREATE TABLE users (c1 TEXT)").expect("CREATE");
    db.execute("INSERT INTO users (c1) VALUES ('Javier')").expect("INSERT 1");
    db.execute("INSERT INTO users (c1) VALUES ('Ueli')").expect("INSERT 2");
    
    db.execute("CREATE BRANCH dev1 AS OF NOW").expect("CREATE BRANCH");
    db.execute("USE BRANCH dev1").expect("USE dev1");
    
    // Check branch_id resolution
    let branch = db.storage.get_current_branch();
    eprintln!("*** current_branch = {:?}", branch);
    if let Some(bm) = db.storage.branch_manager() {
        match bm.get_branch_by_name("dev1") {
            Ok(meta) => eprintln!("*** branch 'dev1' found: id={}", meta.branch_id),
            Err(e) => eprintln!("*** branch 'dev1' NOT FOUND: {}", e),
        }
    } else {
        eprintln!("*** branch_manager() returned None!");
    }
    
    db.execute("UPDATE users SET c1='Pedro' WHERE c1='Ueli'").expect("UPDATE");
    
    // Dump keys to see what happened
    let prefix_bytes = b"data:users:";
    let db_arc = db.storage.db();
    let iter = db_arc.iterator(rocksdb::IteratorMode::From(prefix_bytes.as_ref(), rocksdb::Direction::Forward));
    for item in iter {
        if let Ok((key, _)) = item {
            if !key.starts_with(prefix_bytes) { break; }
            let key_str = String::from_utf8_lossy(&key);
            eprintln!("*** data key: {}", key_str);
        }
    }
    let prefix_bytes = b"bdata:";
    let iter = db_arc.iterator(rocksdb::IteratorMode::From(prefix_bytes.as_ref(), rocksdb::Direction::Forward));
    let mut found = false;
    for item in iter {
        if let Ok((key, _)) = item {
            if !key.starts_with(prefix_bytes) { break; }
            let key_str = String::from_utf8_lossy(&key);
            eprintln!("*** bdata key: {}", key_str);
            found = true;
        }
    }
    if !found { eprintln!("*** NO bdata keys found"); }
}
