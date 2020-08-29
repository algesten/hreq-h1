use async_std::net::{TcpListener, TcpStream};
use hreq_h1::server::Connection;
use hreq_h1::Error;

#[async_std::main]
async fn main() -> Result<(), Error> {
    let mut l = TcpListener::bind("127.0.0.1:3000").await?;

    println!("Listening to {:?}", l.local_addr().unwrap());
    listen(&mut l).await;

    Ok(())
}

// #[tracing::instrument(skip(l))]
async fn listen(l: &mut TcpListener) {
    loop {
        let (tcp, _) = l.accept().await.expect("Accept incoming");

        let conn = hreq_h1::server::handshake(tcp);

        let task = async move {
            handle_conn(conn).await.ok();
        };

        async_std::task::spawn(task);
    }
}

async fn handle_conn(mut conn: Connection<TcpStream>) -> Result<(), Error> {
    while let Some(x) = conn.accept().await {
        let (_, respond) = x?;

        let resp = http::Response::builder()
            .header("content-type", "text/plain")
            .header("content-length", 13)
            .body(())
            .unwrap();

        let mut send_body = respond.send_response(resp, false).await?;

        send_body.send_data(b"Hello world!\n", true).await?;
    }

    Ok(())
}
