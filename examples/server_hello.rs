use async_std::net::{TcpListener, TcpStream};
use hreq_h1::server::Connection;
use hreq_h1::Error;

#[async_std::main]
async fn main() -> Result<(), Error> {
    // use tracing::Level;
    // use tracing_subscriber::FmtSubscriber;

    // let sub = FmtSubscriber::builder()
    //     .with_max_level(Level::TRACE)
    //     .finish();

    // tracing::subscriber::set_global_default(sub).expect("tracing set_global_default");

    let l = TcpListener::bind("127.0.0.1:3000").await?;

    println!("Listening to {:?}", l.local_addr().unwrap());

    loop {
        let (tcp, _) = l.accept().await.expect("Accept incoming");

        let conn = hreq_h1::server::handshake(tcp);

        async_std::task::spawn(async move {
            handle_conn(conn).await.ok();
        });
    }
}

async fn handle_conn(mut conn: Connection<TcpStream>) -> Result<(), Error> {
    while let Some(x) = conn.accept().await {
        let (_, respond) = x.expect("Handshaken");

        let resp = http::Response::builder()
            .header("content-type", "text/plain")
            .header("content-length", 13)
            .body(())
            .unwrap();

        let send_body = respond.send_response(resp, false)?;

        let mut send_body = send_body.ready().await?;

        send_body.send_data(b"Hello world!\n", true).await?;
    }

    Ok(())
}
