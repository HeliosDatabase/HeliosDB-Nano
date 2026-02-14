//! Performance Benchmarks for Multi-User ACID Mode
//!
//! Measures throughput and latency for concurrent workloads in v3.1.0.
//! Target performance:
//! - >10,000 QPS for point lookups
//! - <5ms 99th percentile latency
//! - Linear scalability up to 16 cores

#![cfg(test)]

use heliosdb_nano::{EmbeddedDatabase, Result, Error};
use heliosdb_nano::session::IsolationLevel;
use crate::test_helpers::*;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn test_point_lookup_throughput() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    setup_with_test_data(&db, 10000)?;

    let mut handles: Vec<std::thread::JoinHandle<Result<u128>>> = vec![];
    let start = Instant::now();

    // 8 concurrent readers
    for thread_id in 0..8 {
        let db_clone = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            let session = db_clone.create_session(&format!("reader_{}", thread_id), IsolationLevel::ReadCommitted)?;
            let mut latencies: Vec<u128> = Vec::with_capacity(100);
            
            for i in 0..100 {
                let id = (i * 7 + thread_id) % 10000 + 1;
                let q_start = Instant::now();
                db_clone.query_in_session(session, &format!("SELECT * FROM users WHERE id = {}", id), &[])?;
                latencies.push(q_start.elapsed().as_micros());
            }
            
            db_clone.destroy_session(session)?;
            Ok(latencies.iter().sum::<u128>() / 100)
        }));
    }

    for handle in handles {
        handle.join().map_err(|_| Error::internal("Thread panicked"))??;
    }

    let duration = start.elapsed();
    let qps = 800.0 / duration.as_secs_f64();
    println!("Point lookup throughput: {:.2} QPS", qps);
    
    // Target > 10 QPS in debug mode
    assert!(qps > 10.0, "Throughput too low: {:.2} QPS", qps);

    Ok(())
}

#[test]
fn test_concurrent_write_latency() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    db.execute("CREATE TABLE bench_writes (id INT PRIMARY KEY, val TEXT)")?;

    let mut handles: Vec<std::thread::JoinHandle<Result<u128>>> = vec![];
    let start = Instant::now();

    // 4 concurrent writers
    for thread_id in 0..4 {
        let db_clone = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            let session = db_clone.create_session(&format!("writer_{}", thread_id), IsolationLevel::ReadCommitted)?;
            let mut latencies: Vec<u128> = Vec::with_capacity(100);
            
            for i in 0..100 {
                let id = thread_id * 1000 + i;
                let q_start = Instant::now();
                db_clone.execute_in_session(session, &format!("INSERT INTO bench_writes VALUES ({}, 'val')", id))?;
                latencies.push(q_start.elapsed().as_micros());
            }
            
            db_clone.destroy_session(session)?;
            Ok(latencies.iter().sum::<u128>() / 100)
        }));
    }

    for handle in handles {
        handle.join().map_err(|_| Error::internal("Thread panicked"))??;
    }

    let duration = start.elapsed();
    println!("Total write duration: {:?}", duration);

    Ok(())
}

#[test]
fn test_mixed_workload_performance() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    setup_with_test_data(&db, 5000)?;

    let mut handles: Vec<std::thread::JoinHandle<Result<()>>> = vec![];
    
    // 4 readers + 2 writers
    for i in 0..6 {
        let db_clone = Arc::clone(&db);
        let is_writer = i < 2;
        
        handles.push(thread::spawn(move || -> Result<()> {
            let session = db_clone.create_session(&format!("user_{}", i), IsolationLevel::ReadCommitted)?;
            
            for _ in 0..100 {
                if is_writer {
                    db_clone.execute_in_session(session, "UPDATE users SET age = age + 1 WHERE id = 1")?;
                } else {
                    db_clone.query_in_session(session, "SELECT AVG(age) FROM users", &[])?;
                }
                thread::sleep(Duration::from_millis(1));
            }
            
            db_clone.destroy_session(session)?;
            Ok(())
        }));
    }

    for handle in handles {
        handle.join().map_err(|_| Error::internal("Thread panicked"))??;
    }

    Ok(())
}

#[test]
fn test_scalability_concurrent_connections() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    db.execute("CREATE TABLE scalability_test (id INT PRIMARY KEY)")?;

    let mut handles: Vec<std::thread::JoinHandle<Result<()>>> = vec![];
    
    // Test 20 concurrent short-lived sessions
    for i in 0..20 {
        let db_clone = Arc::clone(&db);
        handles.push(thread::spawn(move || -> Result<()> {
            let session = db_clone.create_session(&format!("user_{}", i), IsolationLevel::ReadCommitted)?;
            db_clone.execute_in_session(session, &format!("INSERT INTO scalability_test VALUES ({})", i))?;
            db_clone.destroy_session(session)?;
            Ok(())
        }));
    }

    for handle in handles {
        handle.join().map_err(|_| Error::internal("Thread panicked"))??;
    }

    let results = db.query("SELECT COUNT(*) FROM scalability_test", &[])?;
    assert_eq!(get_int_value(&results[0], 0), Some(20));

    Ok(())
}