use bytes::{Buf, BufMut, BytesMut};
use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

async fn setup_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    (addr, handle)
}

fn put_cstr(buf: &mut BytesMut, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.put_u8(0);
}

fn startup_message() -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(196_608);
    put_cstr(&mut body, "user");
    put_cstr(&mut body, "postgres");
    put_cstr(&mut body, "database");
    put_cstr(&mut body, "postgres");
    put_cstr(&mut body, "client_encoding");
    put_cstr(&mut body, "UTF8");
    body.put_u8(0);

    let mut msg = BytesMut::new();
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn frontend_message(tag: u8, body: BytesMut) -> BytesMut {
    let mut msg = BytesMut::new();
    msg.put_u8(tag);
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn query_message(sql: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, sql);
    frontend_message(b'Q', body)
}

fn parse_message(statement: &str, sql: &str, param_type_oids: &[i32]) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, statement);
    put_cstr(&mut body, sql);
    body.put_i16(param_type_oids.len() as i16);
    for oid in param_type_oids {
        body.put_i32(*oid);
    }
    frontend_message(b'P', body)
}

fn bind_binary_int4_message(portal: &str, statement: &str, value: i32) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    put_cstr(&mut body, statement);

    body.put_i16(1); // one parameter format code
    body.put_i16(1); // binary parameter

    body.put_i16(1); // one parameter
    body.put_i32(4);
    body.extend_from_slice(&value.to_be_bytes());

    body.put_i16(1); // one result format code
    body.put_i16(1); // binary result

    frontend_message(b'B', body)
}

fn describe_portal_message(portal: &str) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_u8(b'P');
    put_cstr(&mut body, portal);
    frontend_message(b'D', body)
}

fn execute_message(portal: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, portal);
    body.put_i32(0);
    frontend_message(b'E', body)
}

fn sync_message() -> BytesMut {
    frontend_message(b'S', BytesMut::new())
}

async fn read_backend_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    timeout(IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .expect("read header timeout")
        .expect("read header");
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len - 4];
    timeout(IO_TIMEOUT, stream.read_exact(&mut body))
        .await
        .expect("read body timeout")
        .expect("read body");
    (header[0], body)
}

async fn read_until_ready(stream: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut messages = Vec::new();
    loop {
        let message = read_backend_message(stream).await;
        let done = message.0 == b'Z';
        messages.push(message);
        if done {
            return messages;
        }
    }
}

async fn send_simple_query(stream: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    timeout(IO_TIMEOUT, stream.write_all(&query_message(sql)))
        .await
        .expect("write query timeout")
        .expect("write query");
    read_until_ready(stream).await
}

fn read_cstr(body: &mut &[u8]) -> String {
    let end = body.iter().position(|byte| *byte == 0).expect("cstring terminator");
    let value = std::str::from_utf8(&body[..end]).expect("utf8 cstring").to_string();
    *body = &body[end + 1..];
    value
}

fn row_description_type_and_format(body: &[u8]) -> (i32, i16) {
    let mut cursor = body;
    let field_count = cursor.get_i16();
    assert_eq!(field_count, 1, "expected one result column");
    let name = read_cstr(&mut cursor);
    assert_eq!(name, "id");
    let _table_oid = cursor.get_i32();
    let _column_attr_num = cursor.get_i16();
    let type_oid = cursor.get_i32();
    let _data_type_size = cursor.get_i16();
    let _type_modifier = cursor.get_i32();
    let format_code = cursor.get_i16();
    (type_oid, format_code)
}

fn single_data_row_value(body: &[u8]) -> Vec<u8> {
    let mut cursor = body;
    let column_count = cursor.get_i16();
    assert_eq!(column_count, 1, "expected one data column");
    let len = cursor.get_i32();
    assert!(len >= 0, "expected non-null int4 value");
    let len = len as usize;
    assert!(
        cursor.remaining() >= len,
        "data row length says {len} bytes but only {} remain",
        cursor.remaining()
    );
    cursor[..len].to_vec()
}

async fn connect_wire(addr: SocketAddr) -> TcpStream {
    let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port());
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(target))
        .await
        .expect("connect timeout")
        .expect("connect");
    timeout(IO_TIMEOUT, stream.write_all(&startup_message()))
        .await
        .expect("startup write timeout")
        .expect("startup write");
    let startup = read_until_ready(&mut stream).await;
    assert!(
        startup.iter().any(|(tag, _)| *tag == b'R'),
        "startup should include AuthenticationOk"
    );
    stream
}

async fn run_binary_int4_select(stream: &mut TcpStream, value: i32) -> Vec<(u8, Vec<u8>)> {
    let mut batch = BytesMut::new();
    batch.extend_from_slice(&parse_message("", "SELECT id FROM a15_wire WHERE id = $1", &[23]));
    batch.extend_from_slice(&bind_binary_int4_message("", "", value));
    batch.extend_from_slice(&describe_portal_message(""));
    batch.extend_from_slice(&execute_message(""));
    batch.extend_from_slice(&sync_message());

    timeout(IO_TIMEOUT, stream.write_all(&batch))
        .await
        .expect("extended batch write timeout")
        .expect("extended batch write");
    read_until_ready(stream).await
}

#[tokio::test]
async fn a15_binary_int4_param_and_result_work_inside_transaction() {
    let (addr, server_handle) = setup_server().await;
    let mut stream = connect_wire(addr).await;

    send_simple_query(
        &mut stream,
        "CREATE TABLE a15_wire (id INT PRIMARY KEY); INSERT INTO a15_wire VALUES (1);",
    )
    .await;
    let begin = send_simple_query(&mut stream, "BEGIN").await;
    assert_eq!(begin.last().expect("ready").1, vec![b'T']);

    let messages = run_binary_int4_select(&mut stream, 1).await;
    assert!(
        !messages.iter().any(|(tag, _)| *tag == b'E'),
        "extended binary int4 SELECT returned ErrorResponse"
    );

    let row_description = messages
        .iter()
        .find_map(|(tag, body)| (*tag == b'T').then_some(body))
        .expect("RowDescription");
    let (type_oid, format_code) = row_description_type_and_format(row_description);
    assert_eq!(type_oid, 23, "id should be described as int4");
    assert_eq!(
        format_code, 1,
        "Describe Portal must report binary format when Bind requested binary int4 results"
    );

    let data_row = messages
        .iter()
        .find_map(|(tag, body)| (*tag == b'D').then_some(body))
        .expect("DataRow");
    let value = single_data_row_value(data_row);
    assert_eq!(
        value,
        1i32.to_be_bytes(),
        "DataRow int4 payload must be the four-byte network-endian binary representation"
    );
    assert_eq!(messages.last().expect("ready").1, vec![b'T']);

    send_simple_query(&mut stream, "COMMIT").await;
    server_handle.abort();
}

#[tokio::test]
async fn a15_binary_int4_transaction_stress() {
    let (addr, server_handle) = setup_server().await;
    let mut stream = connect_wire(addr).await;

    send_simple_query(
        &mut stream,
        "CREATE TABLE a15_wire (id INT PRIMARY KEY); INSERT INTO a15_wire VALUES (1);",
    )
    .await;
    send_simple_query(&mut stream, "BEGIN").await;

    let started = Instant::now();
    for _ in 0..100 {
        let messages = run_binary_int4_select(&mut stream, 1).await;
        let data_row = messages
            .iter()
            .find_map(|(tag, body)| (*tag == b'D').then_some(body))
            .expect("DataRow");
        assert_eq!(single_data_row_value(data_row), 1i32.to_be_bytes());
    }
    let elapsed = started.elapsed();
    eprintln!("A15 stress: 100 binary int4 extended selects in txn completed in {elapsed:?}");

    send_simple_query(&mut stream, "ROLLBACK").await;
    server_handle.abort();
}
